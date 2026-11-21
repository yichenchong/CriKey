//! Single preference mutation/publication owner. Call persistence off the paint thread.
use crate::command_registry::{CommandRegistry, CommandRegistrySnapshot, RegisteredCommand};
use crikey_config::{
    command_preferences::{CommandPreference, CommandPreferences, KEY_COMMAND_PREFERENCES},
    ConfigError, ConfigLayer, ConfigRevision, ConfigStore,
};
use crikey_core::{command::CommandRef, ArgumentPolicy};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CommandPreferencesSnapshot {
    pub revision: u64,
    pub preferences: CommandPreferences,
    pub layer: Option<ConfigLayer>,
}
#[derive(Debug)]
pub enum PreferenceError {
    Conflict { current_revision: u64 },
    Invalid(String),
    Persistence(String),
    PersistedReload,
    RevisionExhausted,
}
#[derive(Debug, Clone)]
pub enum PreferenceMutation {
    SetRecord(CommandPreference),
    SetAlias {
        reference: CommandRef,
        alias: Option<String>,
    },
    SetHotkey {
        reference: CommandRef,
        hotkey: Option<String>,
    },
    RemoveRecord(CommandRef),
    SetFavorites(Vec<CommandRef>),
    SetFallbacks(Vec<CommandRef>),
}
// Shadowed editable documents still participate in optimistic concurrency:
// an unchanged effective snapshot must not authorize overwriting an external durable edit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditableDocuments {
    user_global: Option<CommandPreferences>,
    session: Option<CommandPreferences>,
}
impl EditableDocuments {
    fn read(config: &ConfigStore) -> Result<Self, PreferenceError> {
        Ok(Self {
            user_global: config
                .command_preferences_at(ConfigLayer::UserGlobal)
                .map_err(PreferenceError::Invalid)?,
            session: config
                .command_preferences_at(ConfigLayer::SessionOverride)
                .map_err(PreferenceError::Invalid)?,
        })
    }
}
#[derive(Debug)]
pub struct CommandPreferencesStore {
    snapshot: Arc<CommandPreferencesSnapshot>,
    editable_documents: EditableDocuments,
    config_revision: Option<ConfigRevision>,
}
impl CommandPreferencesStore {
    /// Empty published state until the configuration authority provides its first load.
    pub fn empty() -> Self {
        Self {
            snapshot: Arc::new(CommandPreferencesSnapshot {
                revision: 0,
                preferences: CommandPreferences::default(),
                layer: None,
            }),
            editable_documents: EditableDocuments {
                user_global: None,
                session: None,
            },
            config_revision: None,
        }
    }
    pub fn load(config: &ConfigStore) -> Result<Self, PreferenceError> {
        Ok(Self {
            snapshot: Arc::new(CommandPreferencesSnapshot {
                revision: 1,
                preferences: read(config)?,
                layer: config.layer_of(KEY_COMMAND_PREFERENCES),
            }),
            editable_documents: EditableDocuments::read(config)?,
            config_revision: Some(config.revision()),
        })
    }
    pub fn snapshot(&self) -> Arc<CommandPreferencesSnapshot> {
        Arc::clone(&self.snapshot)
    }
    /// Complete coalesced snapshot; any revision gap reloads this, never applies partial deltas.
    pub fn changes_since(&self, revision: u64) -> Option<Arc<CommandPreferencesSnapshot>> {
        (revision != self.snapshot.revision).then(|| self.snapshot())
    }
    /// Reconcile external configuration through the same complete-snapshot publication path.
    pub fn reload(
        &mut self,
        config: &ConfigStore,
    ) -> Result<Arc<CommandPreferencesSnapshot>, PreferenceError> {
        let preferences = read(config)?;
        let layer = config.layer_of(KEY_COMMAND_PREFERENCES);
        let editable_documents = EditableDocuments::read(config)?;
        let config_revision = config.revision();
        if preferences == self.snapshot.preferences
            && layer == self.snapshot.layer
            && editable_documents == self.editable_documents
            && self.config_revision.as_ref() == Some(&config_revision)
        {
            return Ok(self.snapshot());
        }
        let published = self.publish(preferences, layer, editable_documents)?;
        self.config_revision = Some(config_revision);
        Ok(published)
    }
    fn publish(
        &mut self,
        preferences: CommandPreferences,
        layer: Option<ConfigLayer>,
        editable_documents: EditableDocuments,
    ) -> Result<Arc<CommandPreferencesSnapshot>, PreferenceError> {
        let revision = self
            .snapshot
            .revision
            .checked_add(1)
            .ok_or(PreferenceError::RevisionExhausted)?;
        self.snapshot = Arc::new(CommandPreferencesSnapshot {
            revision,
            preferences,
            layer,
        });
        self.editable_documents = editable_documents;
        Ok(self.snapshot())
    }
    pub fn apply(
        &mut self,
        config: &mut ConfigStore,
        registry: &CommandRegistry,
        expected_revision: u64,
        mutation: PreferenceMutation,
        session: bool,
    ) -> Result<Arc<CommandPreferencesSnapshot>, PreferenceError> {
        self.apply_snapshot(config, &registry.snapshot(), expected_revision, mutation, session)
    }
    pub fn apply_snapshot(
        &mut self,
        config: &mut ConfigStore,
        registry: &CommandRegistrySnapshot,
        expected_revision: u64,
        mutation: PreferenceMutation,
        session: bool,
    ) -> Result<Arc<CommandPreferencesSnapshot>, PreferenceError> {
        self.reload(config)?;
        if expected_revision != self.snapshot.revision {
            return Err(PreferenceError::Conflict {
                current_revision: self.snapshot.revision,
            });
        }
        self.snapshot
            .revision
            .checked_add(1)
            .ok_or(PreferenceError::RevisionExhausted)?;
        let target = if session {
            ConfigLayer::SessionOverride
        } else {
            ConfigLayer::UserGlobal
        };
        let mut preferences = config
            .command_preferences_at(target)
            .map_err(PreferenceError::Invalid)?
            .unwrap_or_default();
        let declared = |reference: &CommandRef| {
            registry.commands.contains_key(reference)
                || self
                    .snapshot
                    .preferences
                    .records
                    .iter()
                    .any(|record| &record.reference == reference)
                || self.snapshot.preferences.favorites.contains(reference)
                || self.snapshot.preferences.fallbacks.contains(reference)
                || preferences
                    .records
                    .iter()
                    .any(|record| &record.reference == reference)
                || preferences.favorites.contains(reference)
                || preferences.fallbacks.contains(reference)
        };
        let valid = match &mutation {
            PreferenceMutation::SetRecord(record) => declared(&record.reference),
            PreferenceMutation::SetAlias { reference, .. }
            | PreferenceMutation::SetHotkey { reference, .. } => declared(reference),
            PreferenceMutation::RemoveRecord(_) => true,
            PreferenceMutation::SetFavorites(list) | PreferenceMutation::SetFallbacks(list) => {
                list.iter().all(declared)
            }
        };
        if !valid {
            return Err(PreferenceError::Invalid(
                "command has no verified durable declaration".into(),
            ));
        }
        if let PreferenceMutation::SetFallbacks(references) = &mutation {
            for reference in references {
                let newly_enabled = !self.snapshot.preferences.fallbacks.contains(reference);
                if newly_enabled
                    && registry.commands.get(reference).is_some_and(|command| {
                        command.declaration.argument_policy == ArgumentPolicy::Forbidden
                    })
                {
                    return Err(PreferenceError::Invalid(
                        "this command does not accept a fallback argument".into(),
                    ));
                }
            }
        }
        let changes_hotkey = matches!(&mutation, PreferenceMutation::SetHotkey { hotkey: Some(_), .. });
        match mutation {
            PreferenceMutation::SetRecord(record) => {
                if let Some(existing) = preferences
                    .records
                    .iter_mut()
                    .find(|existing| existing.reference == record.reference)
                {
                    *existing = record;
                } else {
                    preferences.records.push(record);
                }
            }
            PreferenceMutation::SetAlias { reference, alias } => {
                let record = target_record(&mut preferences, reference);
                record.alias = alias;
            }
            PreferenceMutation::SetHotkey { reference, hotkey } => {
                let record = target_record(&mut preferences, reference);
                record.hotkey = hotkey;
            }
            PreferenceMutation::RemoveRecord(reference) => {
                preferences.records.retain(|record| record.reference != reference)
            }
            PreferenceMutation::SetFavorites(favorites) => preferences.favorites = favorites,
            PreferenceMutation::SetFallbacks(fallbacks) => preferences.fallbacks = fallbacks,
        }
        let encoded = preferences.encode().map_err(PreferenceError::Invalid)?;
        if changes_hotkey {
            if session {
                let mut candidate = config.clone();
                candidate.set_session_override(KEY_COMMAND_PREFERENCES, &encoded);
                candidate
                    .validate_command_hotkey_conflicts()
                    .map_err(PreferenceError::Invalid)?;
            } else {
                let (current, retained) = config
                    .project_user_global_candidates(KEY_COMMAND_PREFERENCES, &encoded)
                    .map_err(|error| PreferenceError::Persistence(error.to_string()))?;
                current
                    .validate_command_hotkey_conflicts()
                    .map_err(PreferenceError::Invalid)?;
                retained
                    .validate_command_hotkey_conflicts()
                    .map_err(PreferenceError::Invalid)?;
            }
        }
        let expected_config = self
            .config_revision
            .clone()
            .ok_or_else(|| PreferenceError::Persistence("configuration authority unavailable".into()))?;
        match config.commit_command_preferences_if_revision(&preferences, session, &expected_config) {
            Ok(()) => {}
            Err(ConfigError::Conflict) => {
                return Err(PreferenceError::Conflict {
                    current_revision: self.snapshot.revision,
                })
            }
            Err(ConfigError::PersistedReload) => return Err(PreferenceError::PersistedReload),
            Err(_) => {
                return Err(PreferenceError::Persistence(
                    "command preferences were not saved".into(),
                ))
            }
        }
        let effective = read(config).map_err(|_| PreferenceError::PersistedReload)?;
        let editable_documents =
            EditableDocuments::read(config).map_err(|_| PreferenceError::PersistedReload)?;
        let published = self.publish(
            effective,
            config.layer_of(KEY_COMMAND_PREFERENCES),
            editable_documents,
        )?;
        self.config_revision = Some(config.revision());
        Ok(published)
    }
    pub fn hidden(&self, reference: &CommandRef) -> bool {
        self.snapshot
            .preferences
            .records
            .iter()
            .any(|record| &record.reference == reference && record.hidden)
    }
    /// Exact aliases precede prefixes; textual aliases remain owned by the existing query layer.
    pub fn aliases<'a>(&'a self, registry: &CommandRegistry, query: &str) -> Vec<&'a CommandRef> {
        self.snapshot.aliases(&registry.snapshot(), query)
    }
    /// Unavailable favorites remain in their saved order for management/home presentation.
    pub fn home<'a>(
        &'a self,
        registry: &'a CommandRegistry,
    ) -> Vec<(&'a CommandRef, Option<&'a RegisteredCommand>)> {
        self.snapshot
            .preferences
            .favorites
            .iter()
            .map(|reference| (reference, registry.resolve(reference)))
            .collect()
    }
    pub fn fallbacks<'a>(
        &'a self,
        registry: &'a CommandRegistry,
        search: &FallbackSearch,
    ) -> Vec<(&'a CommandRef, &'a RegisteredCommand)> {
        if !search.eligible() {
            return Vec::new();
        }
        self.snapshot
            .preferences
            .fallbacks
            .iter()
            .filter_map(|reference| {
                let command = registry.resolve(reference)?;
                (command.declaration.argument_policy != ArgumentPolicy::Forbidden)
                    .then_some((reference, command))
            })
            .collect()
    }
}
impl CommandPreferencesSnapshot {
    /// Projects raw root text against this immutable published input, never a draft.
    pub fn aliases<'a>(&'a self, registry: &CommandRegistrySnapshot, query: &str) -> Vec<&'a CommandRef> {
        if query.is_empty() {
            return Vec::new();
        }
        let query = query.to_lowercase();
        let mut hits: Vec<_> = self
            .preferences
            .records
            .iter()
            .filter_map(|record| {
                if record.hidden {
                    return None;
                }
                let command = registry.commands.get(&record.reference)?;
                if !matches!(
                    command.declaration.availability,
                    crikey_core::CommandAvailability::Available
                ) {
                    return None;
                }
                let alias = record.alias.as_ref()?.to_lowercase();
                alias
                    .starts_with(&query)
                    .then_some((alias != query, &record.reference))
            })
            .collect();
        hits.sort_unstable();
        hits.into_iter().map(|(_, reference)| reference).collect()
    }
}

fn target_record(preferences: &mut CommandPreferences, reference: CommandRef) -> &mut CommandPreference {
    let index = match preferences
        .records
        .iter()
        .position(|record| record.reference == reference)
    {
        Some(index) => index,
        None => {
            preferences.records.push(CommandPreference {
                reference,
                hidden: false,
                alias: None,
                hotkey: None,
            });
            preferences.records.len() - 1
        }
    };
    &mut preferences.records[index]
}
fn read(config: &ConfigStore) -> Result<CommandPreferences, PreferenceError> {
    config
        .get(KEY_COMMAND_PREFERENCES)
        .map(CommandPreferences::decode)
        .transpose()
        .map(|value| value.unwrap_or_default())
        .map_err(PreferenceError::Invalid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdinaryProviderState {
    Pending,
    Empty,
    Matches,
    Failed,
    Cancelled,
}
/// Captured at query admission. The integration must include every applicable ordinary provider.
#[derive(Debug, Clone)]
pub struct FallbackSearch {
    query: String,
    providers: Vec<OrdinaryProviderState>,
}
impl FallbackSearch {
    pub fn new(query: String, applicable_providers: usize) -> Result<Self, &'static str> {
        if query.len() > 4096 || applicable_providers > 10_000 {
            return Err("fallback search exceeds bounds");
        }
        Ok(Self {
            query,
            providers: vec![OrdinaryProviderState::Pending; applicable_providers],
        })
    }
    pub fn update(&mut self, provider: usize, state: OrdinaryProviderState) -> Result<(), &'static str> {
        let slot = self
            .providers
            .get_mut(provider)
            .ok_or("unknown ordinary provider")?;
        if *slot != OrdinaryProviderState::Pending {
            return Err("ordinary provider already terminal");
        }
        *slot = state;
        Ok(())
    }
    pub fn eligible(&self) -> bool {
        !self.query.trim().is_empty()
            && self
                .providers
                .iter()
                .all(|state| *state == OrdinaryProviderState::Empty)
    }
    /// Pass this captured value as one explicit argument, never interpolate shell syntax.
    pub fn argument(&self) -> Option<&str> {
        self.eligible().then_some(self.query.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_fields_preserve_target_record_and_memberships_without_promoting_effective_fields() {
        let reference = crikey_core::command::builtin_commands().remove(0).reference;
        let mut target = CommandPreferences {
            records: vec![CommandPreference {
                reference: reference.clone(),
                hidden: true,
                alias: Some("existing".into()),
                hotkey: Some("Ctrl+Shift+E".into()),
            }],
            favorites: vec![reference.clone()],
            fallbacks: vec![reference.clone()],
        };
        target_record(&mut target, reference.clone()).alias = None;
        assert!(target.records[0].hidden);
        assert_eq!(target.records[0].hotkey.as_deref(), Some("Ctrl+Shift+E"));
        target_record(&mut target, reference.clone()).hotkey = None;
        assert!(target.records[0].hidden);
        assert_eq!(target.favorites, vec![reference.clone()]);
        assert_eq!(target.fallbacks, vec![reference.clone()]);
        let mut absent_target = CommandPreferences::default();
        target_record(&mut absent_target, reference.clone()).alias = Some("new".into());
        assert!(!absent_target.records[0].hidden);
        assert_eq!(absent_target.records[0].hotkey, None);
        assert!(absent_target.favorites.is_empty());
        assert!(absent_target.fallbacks.is_empty());
    }

    #[test]
    fn published_alias_projection_uses_raw_text_exact_then_prefix_and_reference_ties() {
        let mut registry = CommandRegistrySnapshot::default();
        let mut preferences = CommandPreferences::default();
        for (id, alias, hidden, available) in [
            ("z-exact", "go", false, true),
            ("b-prefix", "gone", false, true),
            ("a-prefix", "going", false, true),
            ("hidden", "goblin", true, true),
            ("unavailable", "gold", false, false),
        ] {
            let mut declaration = crikey_core::command::builtin_commands().remove(0);
            declaration.reference.command_id = id.into();
            if !available {
                declaration.availability = crikey_core::CommandAvailability::Unavailable;
            }
            let reference = declaration.reference.clone();
            registry.commands.insert(
                reference.clone(),
                RegisteredCommand {
                    declaration,
                    incarnation: 1,
                },
            );
            preferences.records.push(CommandPreference {
                reference,
                alias: Some(alias.into()),
                hidden,
                hotkey: None,
            });
        }
        let snapshot = CommandPreferencesSnapshot {
            revision: 1,
            preferences,
            layer: None,
        };
        let ids: Vec<_> = snapshot
            .aliases(&registry, "GO")
            .into_iter()
            .map(|reference| reference.command_id.as_str())
            .collect();
        assert_eq!(ids, vec!["z-exact", "a-prefix", "b-prefix"]);
        assert!(snapshot.aliases(&registry, "").is_empty());
        assert!(snapshot.aliases(&registry, "go ").is_empty());
        assert!(snapshot.aliases(&registry, "go argument").is_empty());
    }
    use crikey_platform::{DirectoryConvention, DirectoryEnvironment, StandardDirectories};

    #[test]
    fn durable_edit_does_not_persist_session_favorites() {
        let path = std::env::temp_dir().join(format!("crikey-command-layer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let environment = [
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "CRIKEY_CONFIG_DIR",
            "CRIKEY_DATA_DIR",
            "CRIKEY_CACHE_DIR",
            "CRIKEY_STATE_DIR",
        ]
        .into_iter()
        .fold(DirectoryEnvironment::new(), |environment, key| {
            environment.set(key, path.as_os_str())
        });
        let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment).unwrap();
        let mut config = ConfigStore::load_with_policy(&directories, None).unwrap();
        config.set_user_global("aliases.ss", "Settings");
        let registry = CommandRegistry::with_builtins();
        let reference = registry.snapshot().commands.keys().next().unwrap().clone();
        let mut store = CommandPreferencesStore::load(&config).unwrap();
        store
            .apply(
                &mut config,
                &registry,
                1,
                PreferenceMutation::SetFavorites(vec![reference.clone()]),
                true,
            )
            .unwrap();
        let revision = store.snapshot().revision;
        store
            .apply(
                &mut config,
                &registry,
                revision,
                PreferenceMutation::SetRecord(CommandPreference {
                    reference: reference.clone(),
                    hidden: false,
                    alias: Some("prefs".into()),
                    hotkey: None,
                }),
                false,
            )
            .unwrap();
        assert_eq!(store.snapshot().preferences.favorites, vec![reference.clone()]);
        assert!(
            store.snapshot().preferences.records.is_empty(),
            "session document still wins"
        );
        let restarted = ConfigStore::load_with_policy(&directories, None).unwrap();
        let durable = CommandPreferencesStore::load(&restarted).unwrap().snapshot();
        assert!(durable.preferences.favorites.is_empty());
        assert_eq!(durable.preferences.records[0].alias.as_deref(), Some("prefs"));
        assert_eq!(restarted.get("aliases.ss"), Some("Settings"));
        let before = store.snapshot();
        let unknown = CommandRef {
            provider_id: crikey_core::PluginId("unknown.owner".into()),
            command_id: "never-declared".into(),
        };
        assert!(matches!(
            store.apply(
                &mut config,
                &registry,
                before.revision,
                PreferenceMutation::SetFavorites(vec![unknown]),
                false
            ),
            Err(PreferenceError::Invalid(_))
        ));
        assert_eq!(store.snapshot().revision, before.revision);
        let mut external = ConfigStore::load_with_policy(&directories, None).unwrap();
        let mut external_preferences = external
            .command_preferences_at(ConfigLayer::UserGlobal)
            .unwrap()
            .unwrap();
        external_preferences.records[0].alias = Some("external-durable-edit".into());
        external
            .commit_command_preferences(&external_preferences, false)
            .unwrap();
        let disk_before = std::fs::read(config.config_path()).unwrap();
        assert!(matches!(
            store.apply(
                &mut config,
                &registry,
                before.revision,
                PreferenceMutation::SetRecord(CommandPreference {
                    reference,
                    hidden: true,
                    alias: Some("stale-durable-edit".into()),
                    hotkey: None,
                }),
                false
            ),
            Err(PreferenceError::Conflict { .. })
        ));
        assert_eq!(std::fs::read(config.config_path()).unwrap(), disk_before);
        let restarted = ConfigStore::load_with_policy(&directories, None).unwrap();
        let durable = restarted
            .command_preferences_at(ConfigLayer::UserGlobal)
            .unwrap()
            .unwrap();
        assert_eq!(durable, external_preferences);
        assert!(
            durable.favorites.is_empty(),
            "session-only membership was not promoted by a stale durable edit"
        );
        assert_eq!(store.snapshot().revision, before.revision);
        assert_eq!(store.snapshot().preferences, before.preferences);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn fallback_requires_every_successful_empty_terminal_and_captures_literal_query() {
        let mut search = FallbackSearch::new("a; $(not-a-shell)".into(), 2).unwrap();
        search.update(0, OrdinaryProviderState::Empty).unwrap();
        assert!(!search.eligible());
        search.update(1, OrdinaryProviderState::Failed).unwrap();
        assert!(!search.eligible());
        let mut search = FallbackSearch::new("a; $(not-a-shell)".into(), 1).unwrap();
        search.update(0, OrdinaryProviderState::Empty).unwrap();
        assert_eq!(search.argument(), Some("a; $(not-a-shell)"));
    }
}

#[cfg(test)]
mod shadowed_layer_tests {
    use super::*;
    use crikey_core::PluginId;
    use crikey_platform::{DirectoryConvention, DirectoryEnvironment, StandardDirectories};
    use std::path::PathBuf;

    fn config(name: &str) -> (PathBuf, StandardDirectories, ConfigStore) {
        let path = std::env::temp_dir().join(format!("crikey-command-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let environment = [
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "CRIKEY_CONFIG_DIR",
            "CRIKEY_DATA_DIR",
            "CRIKEY_CACHE_DIR",
            "CRIKEY_STATE_DIR",
        ]
        .into_iter()
        .fold(DirectoryEnvironment::new(), |environment, key| {
            environment.set(key, path.as_os_str())
        });
        let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment).unwrap();
        let config = ConfigStore::load_with_policy(&directories, None).unwrap();
        (path, directories, config)
    }

    #[test]
    fn shadowed_external_edit_conflicts_with_every_stale_durable_mutation() {
        for index in 0..3 {
            let (path, directories, mut config) = config(&format!("shadow-conflict-{index}"));
            config
                .commit_command_preferences(&CommandPreferences::default(), false)
                .unwrap();
            config
                .commit_command_preferences(&CommandPreferences::default(), true)
                .unwrap();
            let registry = CommandRegistry::with_builtins();
            let reference = registry.snapshot().commands.keys().next().unwrap().clone();
            let mut store = CommandPreferencesStore::load(&config).unwrap();
            let original = store.snapshot();
            let external = CommandPreferences {
                favorites: vec![reference.clone()],
                records: vec![CommandPreference {
                    reference: reference.clone(),
                    hidden: true,
                    alias: Some("external".into()),
                    hotkey: None,
                }],
                fallbacks: vec![],
            };
            // Represents an externally reloaded UserGlobal edit beneath the session override.
            config.commit_command_preferences(&external, false).unwrap();
            let mutation = match index {
                0 => PreferenceMutation::SetFavorites(vec![]),
                1 => PreferenceMutation::SetFallbacks(vec![reference.clone()]),
                _ => PreferenceMutation::SetRecord(CommandPreference {
                    reference,
                    hidden: false,
                    alias: Some("stale".into()),
                    hotkey: None,
                }),
            };
            assert!(matches!(
                store.apply(&mut config, &registry, original.revision, mutation, false),
                Err(PreferenceError::Conflict { current_revision }) if current_revision == original.revision + 1
            ));
            assert_eq!(store.snapshot().preferences, original.preferences);
            assert_eq!(store.snapshot().layer, original.layer);
            assert!(store.changes_since(original.revision).is_some());
            assert_eq!(
                config.command_preferences_at(ConfigLayer::UserGlobal).unwrap(),
                Some(external.clone())
            );
            let restarted = ConfigStore::load_with_policy(&directories, None).unwrap();
            assert_eq!(
                restarted.command_preferences_at(ConfigLayer::UserGlobal).unwrap(),
                Some(external)
            );
            std::fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn unavailable_durable_references_remain_manageable_beneath_session_override() {
        let (path, directories, mut config) = config("shadow-unavailable");
        let first = CommandRef {
            provider_id: PluginId("absent.owner".into()),
            command_id: "first".into(),
        };
        let second = CommandRef {
            provider_id: PluginId("absent.owner".into()),
            command_id: "second".into(),
        };
        let record = CommandPreference {
            reference: first.clone(),
            hidden: false,
            alias: Some("retained".into()),
            hotkey: None,
        };
        config
            .commit_command_preferences(
                &CommandPreferences {
                    records: vec![record.clone()],
                    favorites: vec![first.clone(), second.clone()],
                    fallbacks: vec![second.clone(), first.clone()],
                },
                false,
            )
            .unwrap();
        config
            .commit_command_preferences(&CommandPreferences::default(), true)
            .unwrap();
        let registry = CommandRegistry::with_builtins();
        assert!(registry.resolve(&first).is_none());
        let mut store = CommandPreferencesStore::load(&config).unwrap();
        let favorites = vec![second.clone(), first.clone()];
        let fallbacks = vec![first.clone(), second];
        for mutation in [
            PreferenceMutation::SetFavorites(favorites.clone()),
            PreferenceMutation::SetFallbacks(fallbacks.clone()),
            PreferenceMutation::SetRecord(CommandPreference {
                hidden: true,
                ..record
            }),
        ] {
            let revision = store.snapshot().revision;
            store
                .apply(&mut config, &registry, revision, mutation, false)
                .unwrap();
            assert_eq!(store.snapshot().preferences, CommandPreferences::default());
        }
        let revision = store.snapshot().revision;
        let invented = CommandRef {
            provider_id: first.provider_id.clone(),
            command_id: "invented".into(),
        };
        assert!(matches!(
            store.apply(
                &mut config,
                &registry,
                revision,
                PreferenceMutation::SetFavorites(vec![invented]),
                false
            ),
            Err(PreferenceError::Invalid(_))
        ));
        assert_eq!(store.snapshot().revision, revision);
        let restarted = ConfigStore::load_with_policy(&directories, None).unwrap();
        let durable = restarted
            .command_preferences_at(ConfigLayer::UserGlobal)
            .unwrap()
            .unwrap();
        assert_eq!(durable.favorites, favorites);
        assert_eq!(durable.fallbacks, fallbacks);
        assert_eq!(durable.records[0].reference, first);
        assert!(durable.records[0].hidden);
        assert_eq!(durable.records[0].alias.as_deref(), Some("retained"));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn missing_session_target_does_not_copy_lower_layer_fields() {
        let (path, directories, mut config) = config("session-target-isolation");
        let registry = CommandRegistry::with_builtins();
        let reference = registry.snapshot().commands.keys().next().unwrap().clone();
        let durable = CommandPreferences {
            records: vec![CommandPreference {
                reference: reference.clone(),
                hidden: true,
                alias: Some("durable-only".into()),
                hotkey: Some("Ctrl+Alt+D".into()),
            }],
            favorites: Vec::new(),
            fallbacks: vec![reference.clone()],
        };
        config.commit_command_preferences(&durable, false).unwrap();
        let mut store = CommandPreferencesStore::load(&config).unwrap();
        let revision = store.snapshot().revision;
        store
            .apply(
                &mut config,
                &registry,
                revision,
                PreferenceMutation::SetFavorites(vec![reference.clone()]),
                true,
            )
            .unwrap();
        let session = config
            .command_preferences_at(ConfigLayer::SessionOverride)
            .unwrap()
            .unwrap();
        assert_eq!(session.favorites, vec![reference]);
        assert!(session.records.is_empty());
        assert!(session.fallbacks.is_empty());
        assert_eq!(
            config.command_preferences_at(ConfigLayer::UserGlobal).unwrap(),
            Some(durable.clone())
        );
        assert_eq!(
            ConfigStore::load_with_policy(&directories, None)
                .unwrap()
                .command_preferences_at(ConfigLayer::UserGlobal)
                .unwrap(),
            Some(durable)
        );
        std::fs::remove_dir_all(path).unwrap();
    }
}
