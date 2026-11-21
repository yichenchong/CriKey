//! Command identity projection and source-pinned dispatch owned by SearchService.
use crate::{
    command_preferences::{CommandPreferencesSnapshot, FallbackSearch},
    command_registry::{CommandRegistrySnapshot, RegistryError},
    modern_provider::{ModernCommandSnapshot, ModernCommandStatus, ModernOwnerCommands},
    native_provider::{NativeCommandSnapshot, NativeCommandStatus},
    plugin_action::{ActionReceipt, ActionSubmission, DeclaredCommandSource},
    SearchService, SelectionHistoryStore,
};
use crikey_catalog::command_discovery::declared_catalog_item;
use crikey_core::{
    command::{CommandDeclaration, CommandRef, ExecutableTarget, HostCommand},
    ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId,
};
use crikey_query::{Matcher, Normalizer};
use crikey_ranking::RankingSignals;
use crikey_ui::{ActionTarget, CommandRowState, ResultRow, ResultSource, ResultSourceKind};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Default)]
pub struct CommandSourceLedger {
    epoch: u64,
    accepted: BTreeMap<PluginId, AcceptedCatalog>,
    ready: BTreeMap<PluginId, ReadyCommands>,
    native_revisions: BTreeMap<PluginId, u64>,
    modern_revision: u64,
    modern_owners: BTreeMap<PluginId, Arc<ModernOwnerCommands>>,
    disabled: BTreeSet<PluginId>,
}
#[derive(Debug)]
struct AcceptedCatalog {
    instance: u64,
    generation: Generation,
    epoch: u64,
    source: Option<DeclaredCommandSource>,
}
#[derive(Debug)]
struct ReadyCommands {
    source: DeclaredCommandSource,
    declarations: Vec<CommandDeclaration>,
}

#[derive(Debug)]
pub enum CommandSubmission {
    Host(HostCommand),
    Action {
        receipt: ActionReceipt,
        learning: CommandLearning,
    },
}
/// Immutable evidence captured from the same source accepted by provider admission.
/// Retain through completion; never resolve its item again when learning.
#[derive(Debug)]
pub struct CommandLearning {
    pub reference: CommandRef,
    pub item: Item,
    pub query: Option<crikey_query::NormalizedQuery>,
}
#[derive(Debug)]
pub enum CommandInvokeError {
    Stale { current_revision: u64 },
    Unavailable,
    Busy,
    MissingArgument,
    ForbiddenArgument,
    NoItemRanking,
    Admission(crikey_core::CoreError),
    Persistence(std::io::Error),
}
impl std::fmt::Display for CommandInvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale { .. } => f.write_str("command changed; refresh and retry"),
            Self::Unavailable => f.write_str("command is unavailable"),
            Self::Busy => f.write_str("ranking reset is pending; command was not admitted"),
            Self::MissingArgument => f.write_str("command requires an argument"),
            Self::ForbiddenArgument => f.write_str("command does not accept an argument"),
            Self::NoItemRanking => f.write_str("host commands have no learned item ranking"),
            Self::Admission(error) => error.fmt(f),
            Self::Persistence(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for CommandInvokeError {}

/// Supply only an admitted root publication. The caller retains the projection
/// until query, registry, preferences, history or completion evidence changes.
pub struct CommandCompositionContext<'a> {
    pub root: bool,
    pub generation: Generation,
    pub query: &'a str,
    pub compact: bool,
    pub include_favorites: bool,
    pub include_recent: bool,
    pub fallback: Option<&'a FallbackSearch>,
}
impl std::fmt::Debug for CommandCompositionContext<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandCompositionContext")
            .field("root", &self.root)
            .field("generation", &self.generation)
            .field("query", &"[redacted]")
            .field("compact", &self.compact)
            .field("include_favorites", &self.include_favorites)
            .field("include_recent", &self.include_recent)
            .field("has_fallback", &self.fallback.is_some())
            .finish()
    }
}
impl<'a> CommandCompositionContext<'a> {
    pub fn root(generation: Generation, query: &'a str) -> Self {
        Self {
            root: true,
            generation,
            query,
            compact: true,
            include_favorites: true,
            include_recent: true,
            fallback: None,
        }
    }
}

pub fn root_publication_identity(source: &crate::RootSource) -> crikey_ui::PublicationIdentity {
    match source {
        crate::RootSource::Catalog(source) => crikey_ui::PublicationIdentity::Catalog {
            instance: source.instance,
            generation: source.generation,
            producer: source.producer,
            publication: source.publication,
        },
        crate::RootSource::Suggestion { generation, producer } => {
            crikey_ui::PublicationIdentity::Suggestion {
                generation: *generation,
                producer: *producer,
            }
        }
    }
}
fn root_source_from_identity(identity: &crikey_ui::PublicationIdentity) -> crate::RootSource {
    match identity {
        crikey_ui::PublicationIdentity::Catalog {
            instance,
            generation,
            producer,
            publication,
        } => crate::RootSource::Catalog(crate::DeclaredCommandSource {
            instance: *instance,
            generation: *generation,
            producer: *producer,
            publication: *publication,
        }),
        crikey_ui::PublicationIdentity::Suggestion { generation, producer } => {
            crate::RootSource::Suggestion {
                generation: *generation,
                producer: *producer,
            }
        }
    }
}
pub fn root_scope_from_identity(identity: &crikey_ui::ScopeIdentity) -> crate::RootScope {
    crate::RootScope {
        owner: identity.owner.clone(),
        id: identity.id.clone(),
        source: root_source_from_identity(&identity.source),
    }
}

impl SearchService {
    pub fn root_inline_binding(
        &self,
        input: &crikey_ui::InlineArguments,
    ) -> Option<crate::RootExecutableBinding> {
        let binding = if let Some((reference, revision)) = &input.command {
            self.check_command_revision(*revision).ok()?;
            let command = self.command_registry.resolve(reference)?;
            let item = declared_catalog_item(&self.catalog, &command.declaration)?;
            if item.plugin_id != input.executable.target.owner
                || item.stable_id != input.executable.target.item
            {
                return None;
            }
            let source = self
                .command_sources
                .accepted
                .get(&reference.provider_id)?
                .source
                .as_ref()?;
            self.plugin_actions
                .as_ref()?
                .root_binding(&input.executable, Some(source))?
        } else {
            let source = root_source_from_identity(&input.source);
            let canonical = match &source {
                crate::RootSource::Catalog(source) => Some(source),
                _ => None,
            };
            self.plugin_actions
                .as_ref()?
                .root_binding(&input.executable, canonical)?
        };
        (binding.executable == input.executable
            && root_publication_identity(&binding.source) == input.source
            && binding.fingerprint == input.fingerprint)
            .then_some(binding)
    }
    pub fn root_presentations(&self) -> Vec<crate::RootPresentationSnapshot> {
        self.plugin_actions
            .as_ref()
            .map_or_else(Vec::new, |router| router.root_presentations())
    }
    pub fn root_scope_choices(&self) -> Vec<crikey_ui::ScopeChoice> {
        self.root_presentations()
            .into_iter()
            .flat_map(|snapshot| {
                let source = root_publication_identity(&snapshot.source);
                snapshot
                    .presentation
                    .scopes
                    .iter()
                    .map(|scope| crikey_ui::ScopeChoice {
                        identity: crikey_ui::ScopeIdentity {
                            owner: snapshot.owner.clone(),
                            id: scope.id.clone(),
                            source: source.clone(),
                        },
                        label: format!("{} — {}", snapshot.owner.0, scope.label),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    pub fn resolve_root_scope(&self, identity: &crikey_ui::ScopeIdentity) -> Option<crate::RootScope> {
        let scope = root_scope_from_identity(identity);
        self.plugin_actions
            .as_ref()?
            .root_scope_current(&scope)
            .then_some(scope)
    }
    pub fn project_root_rows(
        &self,
        mut rows: Vec<ResultRow>,
        scope: Option<&crikey_ui::ScopeIdentity>,
        limit: Option<usize>,
    ) -> (Vec<ResultRow>, Vec<crikey_ui::SectionHeader>) {
        let snapshots = self.root_presentations();
        let membership = |row: &ResultRow| {
            let source = row.source.as_ref()?;
            let canonical = match source.kind {
                ResultSourceKind::DeclaredCatalog { epoch } => {
                    Some(self.declared_catalog_source(&source.owner, epoch)?)
                }
                ResultSourceKind::Ordinary => None,
            };
            snapshots
                .iter()
                .filter(|snapshot| snapshot.owner == source.owner)
                .filter(|snapshot| {
                    canonical.is_none_or(|canonical| {
                        snapshot.source == crate::RootSource::Catalog(canonical.clone())
                    })
                })
                .filter_map(|snapshot| {
                    snapshot
                        .presentation
                        .items
                        .iter()
                        .find(|item| item.target.item == source.item)
                        .map(|item| (snapshot, item))
                })
                .max_by_key(|(snapshot, _)| matches!(snapshot.source, crate::RootSource::Suggestion { .. }))
        };
        if let Some(scope) = scope {
            let current = self.resolve_root_scope(scope).is_some();
            rows.retain(|row| {
                membership(row).is_some_and(|(snapshot, item)| {
                    let producer_matches = match &scope.source {
                        crikey_ui::PublicationIdentity::Suggestion { producer, .. } => match &snapshot.source
                        {
                            crate::RootSource::Suggestion {
                                producer: current, ..
                            } => current == producer,
                            crate::RootSource::Catalog(source) => source.producer == *producer,
                        },
                        crikey_ui::PublicationIdentity::Catalog { .. } => true,
                    };
                    current
                        && producer_matches
                        && snapshot.owner == scope.owner
                        && item.scopes.contains(&scope.id)
                })
            });
        }
        rows.truncate(limit.unwrap_or(usize::MAX));
        type SectionKey<'a> = (
            &'a PluginId,
            &'a crikey_core::result_presentation::SectionId,
            &'a String,
        );
        let mut groups: Vec<(Option<SectionKey<'_>>, Vec<ResultRow>)> = Vec::new();
        for row in rows {
            let group = membership(&row).and_then(|(snapshot, item)| {
                let id = item.section.as_ref()?;
                let section = snapshot
                    .presentation
                    .sections
                    .iter()
                    .find(|section| &section.id == id)?;
                Some((&snapshot.owner, id, &section.label))
            });
            if let Some((_, rows)) = groups.iter_mut().find(|(key, _)| *key == group) {
                rows.push(row);
            } else {
                groups.push((group, vec![row]));
            }
        }
        let mut rows = Vec::new();
        let mut headers = Vec::new();
        for (group, members) in groups {
            if let Some((owner, id, label)) = group {
                headers.push(crikey_ui::SectionHeader {
                    row: rows.len(),
                    owner: owner.clone(),
                    id: id.clone(),
                    label: label.clone(),
                });
            }
            rows.extend(members);
        }
        (rows, headers)
    }
    pub fn root_command_binding(
        &self,
        reference: &CommandRef,
        revision: u64,
    ) -> Result<Option<crate::RootExecutableBinding>, CommandInvokeError> {
        self.check_command_revision(revision)?;
        let command = self
            .command_registry
            .resolve(reference)
            .ok_or(CommandInvokeError::Unavailable)?;
        let ExecutableTarget::Item { action_id, .. } = &command.declaration.executable_target else {
            return Ok(None);
        };
        let item = declared_catalog_item(&self.catalog, &command.declaration)
            .ok_or(CommandInvokeError::Unavailable)?;
        let source = self
            .command_sources
            .accepted
            .get(&reference.provider_id)
            .and_then(|catalog| catalog.source.as_ref())
            .ok_or(CommandInvokeError::Unavailable)?;
        let key = crikey_core::structured_arguments::ExecutableKey {
            target: crikey_core::structured_arguments::OwnedItem {
                owner: item.plugin_id.clone(),
                item: item.stable_id.clone(),
            },
            action: action_id
                .clone()
                .or_else(|| item.actions.first().map(|action| action.action_id.clone())),
        };
        Ok(self
            .plugin_actions
            .as_ref()
            .and_then(|router| router.root_binding(&key, Some(source))))
    }
    pub fn root_result_binding(
        &self,
        owner: &PluginId,
        item: &ItemId,
        action: Option<&crikey_core::ActionId>,
        source: Option<&ResultSource>,
    ) -> Option<crate::RootExecutableBinding> {
        let key = crikey_core::structured_arguments::ExecutableKey {
            target: crikey_core::structured_arguments::OwnedItem {
                owner: owner.clone(),
                item: item.clone(),
            },
            action: action.cloned(),
        };
        let canonical = match source.map(|source| source.kind) {
            Some(ResultSourceKind::DeclaredCatalog { epoch }) => {
                Some(self.declared_catalog_source(owner, epoch)?)
            }
            _ => None,
        };
        self.plugin_actions.as_ref()?.root_binding(&key, canonical)
    }
    pub fn submit_root_typed(
        &self,
        binding: &crate::RootExecutableBinding,
        values: crikey_core::structured_arguments::ArgumentValues,
    ) -> crikey_core::Result<crate::TypedActionReceipt> {
        if self.command_admission_paused {
            return Err(crikey_core::CoreError::Invalid(
                "typed admission unavailable".into(),
            ));
        }
        self.plugin_actions
            .as_ref()
            .ok_or_else(|| crikey_core::CoreError::Invalid("typed admission unavailable".into()))?
            .submit_typed(binding, values)
    }
    pub fn command_snapshot(&self) -> Arc<CommandRegistrySnapshot> {
        self.command_registry.snapshot()
    }
    pub fn command_preferences_snapshot(&self) -> Arc<CommandPreferencesSnapshot> {
        Arc::clone(&self.command_preferences_snapshot)
    }
    pub fn declared_catalog_source(&self, owner: &PluginId, epoch: u64) -> Option<&DeclaredCommandSource> {
        let accepted = self.command_sources.accepted.get(owner)?;
        (accepted.epoch == epoch)
            .then_some(accepted.source.as_ref())
            .flatten()
    }
    /// Called only after a live provider catalog is accepted, never for cache restoration.
    pub fn accept_command_catalog(
        &mut self,
        owner: &PluginId,
        instance: u64,
        generation: Generation,
    ) -> Result<(), RegistryError> {
        let epoch = self
            .command_sources
            .epoch
            .checked_add(1)
            .ok_or(RegistryError::RevisionExhausted)?;
        self.command_registry.revoke_owner(owner)?;
        self.command_sources.epoch = epoch;
        self.command_sources.accepted.insert(
            owner.clone(),
            AcceptedCatalog {
                instance,
                generation,
                epoch,
                source: None,
            },
        );
        self.reconcile_command_owner(owner)
    }
    /// Retires dispatch authority during replacement without discarding a Ready
    /// publication that may have arrived before its matching catalog.
    pub fn retire_command_catalog_binding(&mut self, owner: &PluginId) -> Result<(), RegistryError> {
        self.command_sources.accepted.remove(owner);
        self.command_registry.revoke_owner(owner)?;
        Ok(())
    }
    /// Configuration admission gate, independent of asynchronous provider delivery.
    pub fn set_command_owner_enabled(
        &mut self,
        owner: &PluginId,
        enabled: bool,
    ) -> Result<(), RegistryError> {
        if enabled {
            if self.command_sources.disabled.remove(owner) {
                self.reconcile_command_owner(owner)
            } else {
                Ok(())
            }
        } else {
            if self.command_sources.disabled.contains(owner) {
                return Ok(());
            }
            self.command_sources.disabled.insert(owner.clone());
            self.withdraw_command_catalog(owner)
        }
    }
    pub fn withdraw_command_catalog(&mut self, owner: &PluginId) -> Result<(), RegistryError> {
        self.command_sources.accepted.remove(owner);
        self.command_sources.ready.remove(owner);
        self.command_registry.revoke_owner(owner)?;
        Ok(())
    }
    fn revoke_command_source(&mut self, owner: &PluginId) -> Result<(), RegistryError> {
        self.command_sources.ready.remove(owner);
        if let Some(accepted) = self.command_sources.accepted.get_mut(owner) {
            accepted.source = None;
        }
        self.command_registry.revoke_owner(owner)?;
        Ok(())
    }
    fn reconcile_command_owner(&mut self, owner: &PluginId) -> Result<(), RegistryError> {
        if self.command_sources.disabled.contains(owner) {
            return Ok(());
        }
        let Some(ready) = self.command_sources.ready.get(owner) else {
            return Ok(());
        };
        let Some(accepted) = self.command_sources.accepted.get(owner) else {
            return Ok(());
        };
        if ready.source.instance != accepted.instance || ready.source.generation != accepted.generation {
            self.command_registry.revoke_owner(owner)?;
            self.command_sources
                .accepted
                .get_mut(owner)
                .expect("accepted catalog remains present")
                .source = None;
            return Ok(());
        }
        if ready.declarations.iter().any(|declaration| {
            &declaration.reference.provider_id != owner
                || declared_catalog_item(&self.catalog, declaration).is_none()
        }) {
            return self.revoke_command_source(owner);
        }
        self.command_registry
            .publish_owner(owner, ready.declarations.clone())?;
        self.command_sources
            .accepted
            .get_mut(owner)
            .expect("accepted catalog remains present")
            .source = Some(ready.source.clone());
        Ok(())
    }
    pub fn intake_native_commands(&mut self, snapshot: &NativeCommandSnapshot) -> Result<(), RegistryError> {
        if self
            .command_sources
            .native_revisions
            .get(&snapshot.plugin)
            .is_some_and(|revision| *revision >= snapshot.revision)
        {
            return Ok(());
        }
        self.command_sources
            .native_revisions
            .insert(snapshot.plugin.clone(), snapshot.revision);
        if !matches!(snapshot.status, NativeCommandStatus::Ready) {
            return self.revoke_command_source(&snapshot.plugin);
        }
        self.command_sources.ready.insert(
            snapshot.plugin.clone(),
            ReadyCommands {
                source: DeclaredCommandSource {
                    instance: snapshot.instance,
                    generation: snapshot.generation,
                    producer: snapshot.producer,
                    publication: snapshot.revision,
                },
                declarations: snapshot.declarations.to_vec(),
            },
        );
        self.reconcile_command_owner(&snapshot.plugin)
    }
    /// Complete modern snapshots are reconciled per owner, not per global revision.
    pub fn intake_modern_commands(&mut self, snapshot: &ModernCommandSnapshot) -> Result<(), RegistryError> {
        if snapshot.revision <= self.command_sources.modern_revision {
            return Ok(());
        }
        let removed: Vec<_> = self
            .command_sources
            .modern_owners
            .keys()
            .filter(|owner| !snapshot.owners.contains_key(*owner))
            .cloned()
            .collect();
        for owner in removed {
            self.revoke_command_source(&owner)?;
        }
        for (owner, record) in &snapshot.owners {
            if self
                .command_sources
                .modern_owners
                .get(owner)
                .is_some_and(|previous| Arc::ptr_eq(previous, record))
            {
                continue;
            }
            match &record.status {
                ModernCommandStatus::Pending => {}
                ModernCommandStatus::Ready => {
                    self.command_sources.ready.insert(
                        owner.clone(),
                        ReadyCommands {
                            source: DeclaredCommandSource {
                                instance: record.instance,
                                generation: record.generation,
                                producer: record.incarnation,
                                publication: record.request_id,
                            },
                            declarations: record.declarations.clone(),
                        },
                    );
                    self.reconcile_command_owner(owner)?;
                }
                _ => self.revoke_command_source(owner)?,
            }
        }
        self.command_sources.modern_owners = snapshot.owners.clone();
        self.command_sources.modern_revision = snapshot.revision;
        Ok(())
    }
    fn check_command_revision(&self, expected: u64) -> Result<(), CommandInvokeError> {
        let current = self.command_snapshot().revision;
        if current != expected {
            return Err(CommandInvokeError::Stale {
                current_revision: current,
            });
        }
        Ok(())
    }
    pub fn invoke_command(
        &self,
        reference: &CommandRef,
        argument: Option<&str>,
        expected_registry_revision: u64,
    ) -> Result<CommandSubmission, CommandInvokeError> {
        if self.command_admission_paused {
            return Err(CommandInvokeError::Busy);
        }
        self.check_command_revision(expected_registry_revision)?;
        let command = self
            .command_registry
            .resolve(reference)
            .ok_or(CommandInvokeError::Unavailable)?;
        if self
            .root_command_binding(reference, expected_registry_revision)?
            .is_some()
        {
            return Err(CommandInvokeError::MissingArgument);
        }
        match command.declaration.argument_policy {
            ArgumentPolicy::Required if argument.is_none_or(str::is_empty) => {
                return Err(CommandInvokeError::MissingArgument)
            }
            ArgumentPolicy::Forbidden if argument.is_some() => {
                return Err(CommandInvokeError::ForbiddenArgument)
            }
            _ => {}
        }
        match &command.declaration.executable_target {
            ExecutableTarget::Host(host) if reference.provider_id.0 == "crikey.host" => {
                Ok(CommandSubmission::Host(*host))
            }
            ExecutableTarget::Host(_) => Err(CommandInvokeError::Unavailable),
            ExecutableTarget::Item { .. } => {
                let item = declared_catalog_item(&self.catalog, &command.declaration)
                    .ok_or(CommandInvokeError::Unavailable)?;
                let source = self
                    .command_sources
                    .accepted
                    .get(&reference.provider_id)
                    .and_then(|catalog| catalog.source.as_ref())
                    .ok_or(CommandInvokeError::Unavailable)?;
                let router = self
                    .plugin_actions
                    .as_ref()
                    .ok_or(CommandInvokeError::Unavailable)?;
                let receipt = router
                    .submit_declared_command_with_receipt(&command.declaration, source, argument)
                    .map_err(CommandInvokeError::Admission)?;
                Ok(CommandSubmission::Action {
                    receipt: ActionReceipt {
                        submission: ActionSubmission::Pending(receipt.request_id),
                        query_history_allowed: receipt.query_history_allowed,
                    },
                    learning: CommandLearning {
                        reference: reference.clone(),
                        item: item.clone(),
                        query: self.command_learning_query(argument),
                    },
                })
            }
        }
    }
    fn command_learning_query(&self, argument: Option<&str>) -> Option<crikey_query::NormalizedQuery> {
        // Argument-bearing roots may contain private literals; empty Home roots
        // also carry no discovery affinity. Both still earn item selection credit.
        if argument.is_some() {
            None
        } else {
            self.last_query
                .as_ref()
                .filter(|query| !query.tokens.is_empty())
                .cloned()
        }
    }
    /// Call only for a terminal successful command, with its retained admission evidence.
    pub fn record_command_learning(&mut self, learning: CommandLearning) -> Result<(), &'static str> {
        self.history
            .record_command_success(&learning.reference, self.now_secs)?;
        if let Some(query) = learning.query.as_ref() {
            self.history.record(&learning.item, query, self.now_secs);
        } else {
            self.history.record_item_selection(&learning.item, self.now_secs);
        }
        self.candidate_cache = None;
        Ok(())
    }
    /// The typed host dispatcher calls this only after its exact invocation succeeds.
    pub fn record_host_command_success(&mut self, reference: &CommandRef) -> Result<(), &'static str> {
        self.history.record_command_success(reference, self.now_secs)
    }
    /// Captures the exact verified current target before an off-thread selective reset.
    pub fn command_ranking_target(
        &self,
        reference: &CommandRef,
        expected_registry_revision: u64,
    ) -> Result<(PluginId, ItemId), CommandInvokeError> {
        self.check_command_revision(expected_registry_revision)?;
        let command = self
            .command_registry
            .resolve(reference)
            .ok_or(CommandInvokeError::Unavailable)?;
        if matches!(command.declaration.executable_target, ExecutableTarget::Host(_)) {
            return Err(CommandInvokeError::NoItemRanking);
        }
        let item = declared_catalog_item(&self.catalog, &command.declaration)
            .ok_or(CommandInvokeError::Unavailable)?;
        if self
            .command_sources
            .accepted
            .get(&reference.provider_id)
            .and_then(|catalog| catalog.source.as_ref())
            .is_none()
        {
            return Err(CommandInvokeError::Unavailable);
        }
        Ok((item.plugin_id.clone(), item.stable_id.clone()))
    }
    /// Blocking persist-before-publish helper; never call from paint or event handling.
    pub fn reset_command_ranking(
        &mut self,
        reference: &CommandRef,
        expected_registry_revision: u64,
        store: &SelectionHistoryStore,
    ) -> Result<bool, CommandInvokeError> {
        let (owner, item) = self.command_ranking_target(reference, expected_registry_revision)?;
        let changed = store
            .reset_item(&mut self.history, &owner, &item)
            .map_err(CommandInvokeError::Persistence)?;
        if changed {
            self.candidate_cache = None;
        }
        Ok(changed)
    }
    pub fn compose_command_rows(&self, context: CommandCompositionContext<'_>) -> Vec<ResultRow> {
        if !context.root || !self.app.generations().is_current(context.generation) {
            return Vec::new();
        }
        let registry = self.command_snapshot();
        let preferences = self.command_preferences_snapshot();
        let mut rows = Vec::new();
        let mut seen = BTreeSet::new();
        if context.query.trim().is_empty() {
            if context.compact {
                return rows;
            }
            if context.include_favorites {
                for reference in &preferences.preferences.favorites {
                    if seen.insert(reference.clone()) {
                        rows.push(self.command_row(reference, &registry, &preferences, None, Vec::new()));
                    }
                }
            }
            if context.include_recent {
                for record in self.history.recent_commands() {
                    if seen.insert(record.reference.clone()) {
                        rows.push(self.command_row(
                            &record.reference,
                            &registry,
                            &preferences,
                            None,
                            Vec::new(),
                        ));
                    }
                }
            }
        } else {
            // Command aliases see the admitted raw text, before textual rewriting.
            for reference in preferences.aliases(&registry, context.query.trim()) {
                let row = self.command_row(reference, &registry, &preferences, None, Vec::new());
                if row.command_state.as_ref().is_some_and(|state| state.available)
                    && seen.insert(reference.clone())
                {
                    rows.push(row);
                }
            }
            let query = self.aliases.expand(self.normalizer.normalize(context.query));
            let affinity = self.history.affinities_for(&query);
            let mut matches = Vec::new();
            for (reference, command) in &registry.commands {
                if self.command_registry.resolve(reference).is_none()
                    || preferences
                        .preferences
                        .records
                        .iter()
                        .any(|record| &record.reference == reference && record.hidden)
                {
                    continue;
                }
                let item = self.command_search_item(&command.declaration);
                let Some(outcome) = self.matcher.match_item(&query, &item) else {
                    continue;
                };
                let mut signals = RankingSignals::default();
                self.history.augment(
                    &item,
                    &affinity,
                    self.now_secs,
                    self.foreground_category.as_ref(),
                    &mut signals,
                );
                let score = self.ranker.score_outcome_with_signals(&item, &outcome, signals);
                matches.push((reference, score, outcome.highlights));
            }
            matches.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
            for (reference, _, highlights) in matches {
                if seen.insert(reference.clone()) {
                    rows.push(self.command_row(reference, &registry, &preferences, None, highlights));
                }
            }
            if let Some(search) = context
                .fallback
                .filter(|search| search.argument() == Some(context.query))
            {
                for reference in &preferences.preferences.fallbacks {
                    let Some(command) = self.command_registry.resolve(reference) else {
                        continue;
                    };
                    if command.declaration.argument_policy == ArgumentPolicy::Forbidden
                        || self
                            .root_command_binding(reference, registry.revision)
                            .ok()
                            .flatten()
                            .is_some()
                        || !search.eligible()
                        || !seen.insert(reference.clone())
                    {
                        continue;
                    }
                    rows.push(self.command_row(
                        reference,
                        &registry,
                        &preferences,
                        search.argument(),
                        Vec::new(),
                    ));
                }
            }
        }
        rows
    }
    /// All exact references for management, including hidden and unavailable
    /// saved records. No target-to-first-command inference is performed.
    pub fn command_management_rows(&self) -> Vec<ResultRow> {
        let registry = self.command_snapshot();
        let preferences = self.command_preferences_snapshot();
        let references: BTreeSet<_> = registry
            .commands
            .keys()
            .chain(
                preferences
                    .preferences
                    .records
                    .iter()
                    .map(|record| &record.reference),
            )
            .chain(preferences.preferences.favorites.iter())
            .chain(preferences.preferences.fallbacks.iter())
            .collect();
        references
            .into_iter()
            .map(|reference| self.command_row(reference, &registry, &preferences, None, Vec::new()))
            .collect()
    }
    /// Whether a canonical catalog primary row is represented by an exact command.
    /// Call only for catalog-origin hits: live suggestions retain their own
    /// discovery semantics even when they reuse this owner/item identity.
    pub fn is_declared_primary_catalog_item(&self, item: &Item) -> bool {
        if self
            .command_sources
            .accepted
            .get(&item.plugin_id)
            .and_then(|catalog| catalog.source.as_ref())
            .is_none()
        {
            return false;
        }
        self.command_snapshot().commands.values().any(|command| {
            let declaration = &command.declaration;
            if declaration.reference.provider_id != item.plugin_id {
                return false;
            }
            let ExecutableTarget::Item { item_id, action_id } = &declaration.executable_target else {
                return false;
            };
            if item_id != &item.stable_id {
                return false;
            }
            let Some(canonical) = declared_catalog_item(&self.catalog, declaration) else {
                return false;
            };
            let primary = canonical.actions.first().map(|action| &action.action_id);
            action_id.as_ref().or(primary) == primary
        })
    }
    fn command_search_item(&self, declaration: &CommandDeclaration) -> Item {
        if let Some(item) = declared_catalog_item(&self.catalog, declaration) {
            let mut search_terms =
                Vec::with_capacity(item.search_terms.len() + usize::from(item.label != declaration.label));
            search_terms.extend(item.search_terms.iter().cloned());
            if item.label != declaration.label {
                search_terms.push(item.label.clone());
            }
            return Item {
                stable_id: item.stable_id.clone(),
                plugin_id: item.plugin_id.clone(),
                category: item.category.clone(),
                label: declaration.label.clone(),
                description: item.description.clone(),
                target: String::new(),
                search_terms,
                icon_reference: None,
                argument_policy: item.argument_policy,
                hit_policy: item.hit_policy,
                score_hint: item.score_hint,
                metadata: BTreeMap::new(),
                actions: Vec::new(),
            };
        }
        Item {
            stable_id: ItemId(String::new()),
            plugin_id: declaration.reference.provider_id.clone(),
            category: Category::Command,
            label: declaration.label.clone(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: declaration.argument_policy,
            hit_policy: HitPolicy::Ignored,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        }
    }
    fn command_row(
        &self,
        reference: &CommandRef,
        registry: &CommandRegistrySnapshot,
        preferences: &CommandPreferencesSnapshot,
        argument: Option<&str>,
        highlights: Vec<(usize, usize)>,
    ) -> ResultRow {
        let command = registry.commands.get(reference);
        let declaration = command.map(|command| &command.declaration);
        let item = declaration.and_then(|declaration| declared_catalog_item(&self.catalog, declaration));
        let accepted = self
            .command_sources
            .accepted
            .get(&reference.provider_id)
            .filter(|catalog| catalog.source.is_some());
        let available = self.command_registry.resolve(reference).is_some()
            && (item.is_some() && accepted.is_some()
                || declaration.is_some_and(|declaration| {
                    matches!(declaration.executable_target, ExecutableTarget::Host(_))
                }));
        let record = preferences
            .preferences
            .records
            .iter()
            .find(|record| &record.reference == reference);
        let policy = declaration.map_or(ArgumentPolicy::Forbidden, |declaration| {
            declaration.argument_policy
        });
        let typed = self
            .root_command_binding(reference, registry.revision)
            .ok()
            .flatten()
            .is_some();
        let policy = if typed { ArgumentPolicy::Forbidden } else { policy };
        ResultRow {
            target: ActionTarget::Command(reference.clone()),
            source: item.zip(accepted).map(|(item, catalog)| ResultSource {
                owner: item.plugin_id.clone(),
                item: item.stable_id.clone(),
                kind: ResultSourceKind::DeclaredCatalog { epoch: catalog.epoch },
            }),
            command_state: Some(CommandRowState {
                registry_revision: registry.revision,
                preference_revision: preferences.revision,
                available,
                unavailable_reason: (!available).then(|| {
                    "Command is unavailable; its installed catalog or declaration is not current".into()
                }),
                favorite: preferences
                    .preferences
                    .favorites
                    .iter()
                    .position(|candidate| candidate == reference),
                favorite_count: preferences.preferences.favorites.len(),
                hidden: record.is_some_and(|record| record.hidden),
                fallback: preferences
                    .preferences
                    .fallbacks
                    .iter()
                    .position(|candidate| candidate == reference),
                fallback_count: preferences.preferences.fallbacks.len(),
                aliases: record
                    .and_then(|record| record.alias.clone())
                    .into_iter()
                    .collect(),
                hotkey: record.and_then(|record| record.hotkey.clone()),
                argument_policy: policy,
                argument: argument.map(str::to_owned),
                ranking_reset_available: available && item.is_some(),
            }),
            label: declaration.map_or_else(
                || format!("{} / {}", reference.provider_id.0, reference.command_id),
                |declaration| declaration.label.clone(),
            ),
            description: item.map_or_else(
                || format!("{} / {}", reference.provider_id.0, reference.command_id),
                |item| {
                    format!(
                        "{} · {} / {}",
                        item.description, reference.provider_id.0, reference.command_id
                    )
                },
            ),
            icon: item
                .and_then(|item| item.icon_reference.as_deref())
                .and_then(|reference| self.icon(reference)),
            icon_reference: item.and_then(|item| item.icon_reference.clone()),
            category: item.map_or("command", |item| item.category.as_str()).to_owned(),
            plugin_name: reference.provider_id.0.clone(),
            highlights,
            argument_hint: match policy {
                ArgumentPolicy::Forbidden => None,
                ArgumentPolicy::Optional => Some("optional argument".into()),
                ArgumentPolicy::Required => Some("argument required".into()),
            },
            status: if typed && preferences.preferences.fallbacks.contains(reference) {
                Some("Saved literal fallback unavailable for typed arguments".into())
            } else {
                (!available).then(|| "Unavailable".into())
            },
            default_action: None,
            alternate_actions: item.map_or_else(Vec::new, |item| item.actions.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> PluginId {
        PluginId("test.commands".into())
    }
    fn record(request_id: u64, status: ModernCommandStatus) -> Arc<ModernOwnerCommands> {
        Arc::new(ModernOwnerCommands {
            request_id,
            instance: 1,
            generation: Generation::from_raw(request_id),
            incarnation: 7,
            status,
            declarations: Vec::new(),
        })
    }
    fn snapshot(revision: u64, record: Arc<ModernOwnerCommands>) -> ModernCommandSnapshot {
        ModernCommandSnapshot {
            revision,
            owners: BTreeMap::from([(owner(), record)]),
        }
    }
    fn service() -> SearchService {
        SearchService::new(crate::App::new())
    }

    #[test]
    fn ready_and_catalog_reconcile_in_either_order() {
        for ready_first in [true, false] {
            let mut search = service();
            let ready = snapshot(1, record(3, ModernCommandStatus::Ready));
            if ready_first {
                search.intake_modern_commands(&ready).unwrap();
            }
            search.retire_command_catalog_binding(&owner()).unwrap();
            search
                .accept_command_catalog(&owner(), 1, Generation::from_raw(3))
                .unwrap();
            if !ready_first {
                search.intake_modern_commands(&ready).unwrap();
            }
            let epoch = search.command_sources.accepted[&owner()].epoch;
            assert_eq!(
                search
                    .declared_catalog_source(&owner(), epoch)
                    .unwrap()
                    .publication,
                3
            );
            assert!(search.declared_catalog_source(&owner(), epoch + 1).is_none());
            let revision = search.command_snapshot().revision;
            search.intake_modern_commands(&ready).unwrap();
            assert_eq!(search.command_snapshot().revision, revision);
        }
    }

    #[test]
    fn pending_retains_only_current_catalog_and_terminal_revokes() {
        let mut search = service();
        search
            .accept_command_catalog(&owner(), 1, Generation::from_raw(1))
            .unwrap();
        let ready = record(1, ModernCommandStatus::Ready);
        search
            .intake_modern_commands(&snapshot(1, ready.clone()))
            .unwrap();
        let epoch = search.command_sources.accepted[&owner()].epoch;
        search
            .intake_modern_commands(&snapshot(2, record(2, ModernCommandStatus::Pending)))
            .unwrap();
        assert!(search.declared_catalog_source(&owner(), epoch).is_some());
        search
            .accept_command_catalog(&owner(), 1, Generation::from_raw(2))
            .unwrap();
        assert!(search.command_sources.accepted[&owner()].source.is_none());
        search
            .intake_modern_commands(&snapshot(3, record(2, ModernCommandStatus::Ready)))
            .unwrap();
        assert!(search.command_sources.accepted[&owner()].source.is_some());
        search
            .intake_modern_commands(&snapshot(
                4,
                record(2, ModernCommandStatus::Failed("failed".into())),
            ))
            .unwrap();
        assert!(search.command_sources.accepted[&owner()].source.is_none());
        assert!(!search.command_sources.ready.contains_key(&owner()));
    }

    #[test]
    fn unchanged_owner_is_not_reconsumed_and_disappearance_revokes() {
        let mut search = service();
        search
            .accept_command_catalog(&owner(), 1, Generation::from_raw(1))
            .unwrap();
        let ready = record(1, ModernCommandStatus::Ready);
        search
            .intake_modern_commands(&snapshot(1, ready.clone()))
            .unwrap();
        let revision = search.command_snapshot().revision;
        let mut changed = snapshot(2, ready);
        changed
            .owners
            .insert(PluginId("other".into()), record(1, ModernCommandStatus::Pending));
        search.intake_modern_commands(&changed).unwrap();
        assert_eq!(search.command_snapshot().revision, revision);
        search
            .intake_modern_commands(&ModernCommandSnapshot {
                revision: 3,
                owners: BTreeMap::new(),
            })
            .unwrap();
        assert!(search.command_sources.accepted[&owner()].source.is_none());
    }

    #[test]
    fn home_defaults_to_compact_and_host_learning_keeps_exact_identity() {
        let mut search = service();
        assert!(CommandCompositionContext::root(Generation::from_raw(1), "").compact);
        let first = CommandRef {
            provider_id: PluginId("crikey.host".into()),
            command_id: "first".into(),
        };
        let second = CommandRef {
            command_id: "second".into(),
            ..first.clone()
        };
        search.record_host_command_success(&first).unwrap();
        search.record_host_command_success(&second).unwrap();
        let recent = search.history.recent_commands();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].reference, first);
        assert_eq!(recent[1].reference, second);
    }

    #[test]
    fn aliases_use_raw_text_exact_before_prefix_and_exclude_hidden_unavailable() {
        let mut search = service();
        let owner = PluginId("crikey.host".into());
        let mut declarations = Vec::new();
        let mut records = Vec::new();
        for (id, alias, hidden, available) in [
            ("a-prefix", "gopher", false, true),
            ("z-exact", "go", false, true),
            ("hidden", "gone", true, true),
            ("missing", "good", false, false),
        ] {
            let reference = CommandRef {
                provider_id: owner.clone(),
                command_id: id.into(),
            };
            declarations.push(CommandDeclaration {
                reference: reference.clone(),
                label: "Unrelated label".into(),
                executable_target: ExecutableTarget::Host(HostCommand::OpenSettings),
                availability: if available {
                    crikey_core::CommandAvailability::Available
                } else {
                    crikey_core::CommandAvailability::Unavailable
                },
                argument_policy: ArgumentPolicy::Forbidden,
            });
            records.push(crikey_config::command_preferences::CommandPreference {
                reference,
                hidden,
                alias: Some(alias.into()),
                hotkey: None,
            });
        }
        search
            .command_registry
            .publish_owner(&owner, declarations)
            .unwrap();
        Arc::make_mut(&mut search.command_preferences_snapshot)
            .preferences
            .records = records;
        let generation = search.app.generations().current();
        let rows = search.compose_command_rows(CommandCompositionContext::root(generation, "GO"));
        let ids: Vec<_> = rows
            .iter()
            .map(|row| match &row.target {
                ActionTarget::Command(reference) => reference.command_id.as_str(),
                _ => panic!("alias must retain command reference"),
            })
            .collect();
        assert_eq!(ids, ["z-exact", "a-prefix"]);
        assert!(search
            .compose_command_rows(CommandCompositionContext::root(generation, "go argument"))
            .is_empty());
    }
}

#[cfg(test)]
mod disabled_owner_tests {
    use super::*;

    #[test]
    fn delayed_ready_and_catalog_cannot_republish_disabled_owner() {
        let mut search = SearchService::new(crate::App::new());
        let owner = PluginId("disabled.commands".into());
        let generation = Generation::from_raw(1);
        search.set_command_owner_enabled(&owner, false).unwrap();
        search
            .intake_modern_commands(&ModernCommandSnapshot {
                revision: 1,
                owners: BTreeMap::from([(
                    owner.clone(),
                    Arc::new(ModernOwnerCommands {
                        request_id: 1,
                        instance: 1,
                        generation,
                        incarnation: 1,
                        status: ModernCommandStatus::Ready,
                        declarations: Vec::new(),
                    }),
                )]),
            })
            .unwrap();
        let revision = search.command_snapshot().revision;
        search.accept_command_catalog(&owner, 1, generation).unwrap();
        assert!(search.command_sources.accepted[&owner].source.is_none());
        assert_eq!(search.command_snapshot().revision, revision);
        search.set_command_owner_enabled(&owner, true).unwrap();
        assert!(search.command_sources.accepted[&owner].source.is_some());
    }
}

#[cfg(test)]
mod canonical_discovery_tests {
    use super::*;
    use crikey_catalog::{CatalogStore, CatalogUpdate};
    use crikey_core::{Action, ActionId, CommandAvailability, ExecutionPolicy};

    #[test]
    fn hidden_primary_replaces_catalog_discovery_but_not_live_suggestions() {
        let mut search = SearchService::new(crate::App::new());
        let owner = PluginId("projection.commands".into());
        let generation = search.app.generations().current();
        let action = |id: &str| Action {
            action_id: ActionId(id.into()),
            label: id.into(),
            description: String::new(),
            applicable_categories: Vec::new(),
            icon_reference: None,
            execution_policy: ExecutionPolicy::Plugin,
        };
        let item = Item {
            stable_id: ItemId("canonical".into()),
            plugin_id: owner.clone(),
            category: Category::Command,
            label: "Original catalog label".into(),
            description: String::new(),
            target: String::new(),
            search_terms: vec!["preserved-keyword".into()],
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: vec![action("primary"), action("alternate")],
        };
        let reference = CommandRef {
            provider_id: owner.clone(),
            command_id: "declared".into(),
        };
        let declaration = CommandDeclaration {
            reference: reference.clone(),
            label: "Declaration label".into(),
            executable_target: ExecutableTarget::Item {
                item_id: item.stable_id.clone(),
                action_id: None,
            },
            availability: CommandAvailability::Available,
            argument_policy: item.argument_policy,
        };
        search.catalog.activate_instance(&owner, 1).unwrap();
        search
            .catalog
            .apply(&owner, 1, CatalogUpdate::Replace, vec![item.clone()])
            .unwrap();
        search
            .intake_modern_commands(&ModernCommandSnapshot {
                revision: 1,
                owners: BTreeMap::from([(
                    owner.clone(),
                    Arc::new(ModernOwnerCommands {
                        request_id: 1,
                        instance: 1,
                        generation,
                        incarnation: 1,
                        status: ModernCommandStatus::Ready,
                        declarations: vec![declaration.clone()],
                    }),
                )]),
            })
            .unwrap();
        search.accept_command_catalog(&owner, 1, generation).unwrap();
        assert!(search.is_declared_primary_catalog_item(&item));
        let projection = search.command_search_item(&declaration);
        for query in ["Original catalog label", "preserved-keyword", "Declaration label"] {
            assert!(search
                .matcher
                .match_item(&search.normalizer.normalize(query), &projection)
                .is_some());
        }
        let mut alternate = declaration.clone();
        alternate.executable_target = ExecutableTarget::Item {
            item_id: item.stable_id.clone(),
            action_id: Some(ActionId("alternate".into())),
        };
        search
            .command_registry
            .publish_owner(&owner, vec![alternate])
            .unwrap();
        assert!(!search.is_declared_primary_catalog_item(&item));
        search
            .command_registry
            .publish_owner(&owner, vec![declaration])
            .unwrap();
        Arc::make_mut(&mut search.command_preferences_snapshot)
            .preferences
            .records
            .push(crikey_config::command_preferences::CommandPreference {
                reference,
                hidden: true,
                alias: None,
                hotkey: None,
            });
        let hit = |item, ephemeral| crate::SearchHit {
            item,
            score: crikey_ranking::Score::new(1.0),
            method: crikey_query::MatchMethod::ExactPrefix,
            highlights: Vec::new(),
            ephemeral,
        };
        let mut suggestion = item.clone();
        suggestion.label = "Live suggestion".into();
        search.results = vec![hit(item, false), hit(suggestion, true)];
        assert!(search
            .compose_command_rows(CommandCompositionContext::root(generation, "Declaration"))
            .is_empty());
        let rows = search.result_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Live suggestion");
    }

    #[test]
    fn represented_hidden_primary_does_not_consume_the_only_ordinary_result_slot() {
        let limits = crate::ResultLimits {
            max_items_per_query: 1,
            max_items_per_plugin_per_query: 1,
            ..Default::default()
        };
        let mut search = SearchService::new(crate::App::with_limits(limits));
        for stage in [
            crate::StartupStage::WindowAndHotkey,
            crate::StartupStage::PersistedCatalog,
            crate::StartupStage::AcceptQueries,
        ] {
            search.complete_stage(stage).unwrap();
        }
        let owner = PluginId("projection.one-slot".into());
        let canonical = Item {
            stable_id: ItemId("canonical".into()),
            plugin_id: owner.clone(),
            category: Category::Command,
            label: "Needle".into(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 1000,
            metadata: BTreeMap::new(),
            actions: vec![Action {
                action_id: ActionId("primary".into()),
                label: "Primary".into(),
                description: String::new(),
                applicable_categories: Vec::new(),
                icon_reference: None,
                execution_policy: ExecutionPolicy::Plugin,
            }],
        };
        let mut ordinary = canonical.clone();
        ordinary.stable_id = ItemId("ordinary".into());
        ordinary.label = "Needle secondary".into();
        ordinary.score_hint = 0;
        let reference = CommandRef {
            provider_id: owner.clone(),
            command_id: "hidden".into(),
        };
        let declaration = CommandDeclaration {
            reference: reference.clone(),
            label: "Hidden command".into(),
            executable_target: ExecutableTarget::Item {
                item_id: canonical.stable_id.clone(),
                action_id: None,
            },
            availability: CommandAvailability::Available,
            argument_policy: ArgumentPolicy::Forbidden,
        };
        let install = |search: &mut SearchService, instance, items| {
            let generation = search.app.generations().current();
            search.replace_catalog(&owner, instance, items).unwrap();
            search
                .intake_modern_commands(&ModernCommandSnapshot {
                    revision: instance,
                    owners: BTreeMap::from([(
                        owner.clone(),
                        Arc::new(ModernOwnerCommands {
                            request_id: instance,
                            instance,
                            generation,
                            incarnation: instance,
                            status: ModernCommandStatus::Ready,
                            declarations: vec![declaration.clone()],
                        }),
                    )]),
                })
                .unwrap();
            search
                .accept_command_catalog(&owner, instance, generation)
                .unwrap();
        };
        install(&mut search, 1, vec![canonical.clone(), ordinary]);
        Arc::make_mut(&mut search.command_preferences_snapshot)
            .preferences
            .records
            .push(crikey_config::command_preferences::CommandPreference {
                reference,
                hidden: true,
                alias: None,
                hotkey: None,
            });
        let generation = search.submit_query("needle").unwrap();
        let rows = search.result_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Needle secondary");
        assert_eq!(
            search.last_query_stats().matches_found,
            2,
            "raw ordinary matches precede representation and capacity"
        );
        assert!(search
            .compose_command_rows(CommandCompositionContext::root(generation, "needle"))
            .is_empty());

        install(&mut search, 2, vec![canonical]);
        search.submit_query("needle").unwrap();
        assert!(search.result_rows().is_empty());
        assert_eq!(
            search.last_query_stats().matches_found,
            1,
            "hidden representation must still block empty-only fallback"
        );
    }
}

#[cfg(test)]
mod command_learning_privacy_tests {
    use super::*;

    #[test]
    fn argument_bearing_root_never_reaches_persisted_affinity() {
        let mut search = SearchService::new(crate::App::new());
        let secret = "private fallback argument 9f6d2c";
        search.last_query = Some(search.normalizer.normalize(secret));
        let declaration = crikey_core::command::builtin_commands().remove(0);
        let mut item = search.command_search_item(&declaration);
        item.stable_id = ItemId("admitted-item".into());
        item.hit_policy = HitPolicy::Recorded;
        for argument in [Some(secret), Some("")] {
            search
                .record_command_learning(CommandLearning {
                    reference: declaration.reference.clone(),
                    item: item.clone(),
                    query: search.command_learning_query(argument),
                })
                .unwrap();
        }
        let snapshot = search.selection_history_snapshot();
        assert!(snapshot.query_affinities.is_empty());
        assert_eq!(snapshot.commands.len(), 1);
        assert_eq!(snapshot.commands[0].reference, declaration.reference);
        assert_eq!(snapshot.commands[0].frequency, 2);
        assert_eq!(snapshot.selections[0].frequency, 2);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "crikey-command-privacy-{}-{unique}.toml",
            std::process::id()
        ));
        let store = SelectionHistoryStore::new(&path);
        store.save(&snapshot).unwrap();
        let persisted = std::fs::read_to_string(&path).unwrap();
        let restored = store.load();
        std::fs::remove_file(path).unwrap();
        assert!(!persisted.contains(secret));
        assert!(restored.query_affinities.is_empty());
        assert_eq!(restored.commands[0].frequency, 2);
        assert_eq!(restored.selections[0].frequency, 2);
    }

    #[test]
    fn hidden_home_favorite_success_persists_selection_without_empty_affinity() {
        use crikey_catalog::{CatalogStore, CatalogUpdate};
        let mut search = SearchService::new(crate::App::new());
        let owner = PluginId("home.learning".into());
        let generation = search.app.generations().current();
        let mut declaration = crikey_core::command::builtin_commands().remove(0);
        declaration.reference.provider_id = owner.clone();
        declaration.reference.command_id = "hidden-favorite".into();
        let mut item = search.command_search_item(&declaration);
        item.stable_id = ItemId("admitted-home-item".into());
        item.hit_policy = HitPolicy::Recorded;
        declaration.executable_target = ExecutableTarget::Item {
            item_id: item.stable_id.clone(),
            action_id: None,
        };
        search.catalog.activate_instance(&owner, 1).unwrap();
        search
            .catalog
            .apply(&owner, 1, CatalogUpdate::Replace, vec![item.clone()])
            .unwrap();
        search
            .intake_modern_commands(&ModernCommandSnapshot {
                revision: 1,
                owners: BTreeMap::from([(
                    owner.clone(),
                    Arc::new(ModernOwnerCommands {
                        request_id: 1,
                        instance: 1,
                        generation,
                        incarnation: 1,
                        status: ModernCommandStatus::Ready,
                        declarations: vec![declaration.clone()],
                    }),
                )]),
            })
            .unwrap();
        search.accept_command_catalog(&owner, 1, generation).unwrap();
        let preferences = &mut Arc::make_mut(&mut search.command_preferences_snapshot).preferences;
        preferences.favorites.push(declaration.reference.clone());
        preferences
            .records
            .push(crikey_config::command_preferences::CommandPreference {
                reference: declaration.reference.clone(),
                hidden: true,
                alias: None,
                hotkey: None,
            });
        let mut context = CommandCompositionContext::root(generation, "");
        context.compact = false;
        let rows = search.compose_command_rows(context);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].target,
            ActionTarget::Command(declaration.reference.clone())
        );
        let state = rows[0].command_state.as_ref().unwrap();
        assert!(state.hidden && state.available);
        assert_eq!(state.favorite, Some(0));
        search.now_secs = 100;
        for root in ["", " \t"] {
            search.last_query = Some(search.normalizer.normalize(root));
            let query = search.command_learning_query(None);
            assert!(query.is_none());
            search
                .record_command_learning(CommandLearning {
                    reference: declaration.reference.clone(),
                    item: item.clone(),
                    query,
                })
                .unwrap();
        }
        let query = search.normalizer.normalize("");
        let affinity = search.history.affinities_for(&query);
        let mut signals = RankingSignals::default();
        search
            .history
            .augment(&item, &affinity, search.now_secs, None, &mut signals);
        assert_eq!(signals.selection_frequency, 2);
        assert_eq!(signals.selection_recency_secs, Some(0));
        assert_eq!(signals.query_history, 0.0);
        let snapshot = search.selection_history_snapshot();
        assert!(snapshot.query_affinities.is_empty());
        assert_eq!(snapshot.commands.len(), 1);
        assert_eq!(snapshot.commands[0].reference, declaration.reference);
        assert_eq!(snapshot.commands[0].frequency, 2);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "crikey-home-learning-{}-{unique}.toml",
            std::process::id()
        ));
        let store = SelectionHistoryStore::new(&path);
        store.save(&snapshot).unwrap();
        let restored = store.load();
        std::fs::remove_file(path).unwrap();
        assert!(restored.query_affinities.is_empty());
        assert_eq!(restored.commands[0].reference, declaration.reference);
        assert_eq!(restored.commands[0].frequency, 2);
        assert_eq!(restored.selections[0].frequency, 2);
        search.last_query = Some(search.normalizer.normalize("nonempty discovery"));
        assert!(search.command_learning_query(None).is_some());
        assert!(search.command_learning_query(Some("argument")).is_none());
    }
}

#[cfg(test)]
mod composition_debug_tests {
    use super::*;

    #[test]
    fn context_debug_never_formats_root_or_fallback_query_text() {
        let fallback = FallbackSearch::new("private fallback argument".into(), 1).unwrap();
        let mut context = CommandCompositionContext::root(Generation::from_raw(1), "private root query");
        context.fallback = Some(&fallback);
        let debug = format!("{context:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("private root query"));
        assert!(!debug.contains("private fallback argument"));
    }
}
