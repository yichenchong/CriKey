//! Configuration edit work is admitted by semantic intent, never performed by paint.
use crikey_app::package_jobs::{JobError, PackageConfiguration};
use crikey_app::{
    launcher_settings,
    settings_search::{SettingsSearch, SettingsSearchResults},
};
use crikey_app::{
    CommandPreferencesSnapshot, CommandPreferencesStore, CommandRegistrySnapshot, PreferenceError,
    PreferenceMutation,
};
#[cfg(test)]
use crikey_config::command_preferences::KEY_COMMAND_PREFERENCES;
use crikey_config::{ConfigError, DiscoveredSchema};
use crikey_config::{ConfigLayer, ConfigRevision, ConfigSourceWatch, ConfigStore};
use crikey_core::PluginId;
use crikey_platform::StandardDirectories;
use std::path::PathBuf;

pub(crate) enum SettingsTarget {
    UserGlobal,
    PluginUser { plugin: PluginId },
}
pub(crate) enum SettingsOperation {
    Search {
        text: String,
    },
    BrowsePlugin {
        plugin: PluginId,
    },
    Edit {
        key: String,
        target: SettingsTarget,
        draft: String,
    },
}
pub(crate) struct SettingsRequest {
    pub view: crikey_ui::ViewId,
    pub request_id: u64,
    pub expected_config: u64,
    pub expected_schema: u64,
    pub operation: SettingsOperation,
}
pub(crate) struct SettingsRow {
    pub key: String,
    pub value: String,
    pub source: String,
    pub requires_restart: bool,
    pub secret: bool,
    /// False means no editable effective value was projected; `value` is not a draft.
    pub value_available: bool,
}
pub(crate) enum SettingsPayload {
    Search {
        results: SettingsSearchResults,
        rows: Vec<SettingsRow>,
    },
    Saved {
        row: SettingsRow,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsFailure {
    Conflict,
    StaleSchema,
    Unavailable,
    Invalid,
    PersistedReload,
}
pub(crate) struct SettingsResponse {
    pub view: crikey_ui::ViewId,
    pub request_id: u64,
    pub config_revision: u64,
    pub schema_revision: u64,
    pub result: Result<SettingsPayload, SettingsFailure>,
}
#[derive(Default)]
struct SettingsAuthority {
    roots: Vec<PathBuf>,
    policy: Option<PathBuf>,
    schemas: Vec<DiscoveredSchema>,
    config_revision: u64,
    schema_revision: u64,
}

struct PackageConfigurationClient {
    sender: mpsc::SyncSender<Request>,
}
impl PackageConfigurationClient {
    fn request(&self, change: Option<(PluginId, bool)>) -> Result<ConfigStore, JobError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.sender
            .try_send(Request::Package { change, reply })
            .map_err(|_| {
                JobError::new(
                    "configuration-unavailable",
                    "Configuration authority is unavailable or busy",
                )
            })?;
        response
            .recv()
            .map_err(|_| JobError::new("configuration-unavailable", "Configuration authority is closed"))?
    }
}
impl PackageConfiguration for PackageConfigurationClient {
    fn load(&self) -> Result<ConfigStore, JobError> {
        self.request(None)
    }
    fn set_plugin_enabled(&self, plugin: &PluginId, enabled: bool) -> Result<ConfigStore, JobError> {
        self.request(Some((plugin.clone(), enabled)))
    }
}
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

pub(crate) struct MutationRequest {
    pub request_id: u64,
    pub config: ConfigStore,
    pub registry: Arc<CommandRegistrySnapshot>,
    pub expected_revision: u64,
    pub reference: crikey_core::CommandRef,
    pub edit: crikey_ui::CommandPreferenceEdit,
    pub session: bool,
}

pub(crate) enum Request {
    Poll,
    Settings(SettingsRequest),
    Package {
        change: Option<(PluginId, bool)>,
        reply: mpsc::SyncSender<Result<ConfigStore, JobError>>,
    },
    Mutate(MutationRequest),
    Setting {
        config: ConfigStore,
        request_id: u64,
        key: String,
        value: String,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionState {
    Authoritative,
    Unavailable,
    PersistedReloadRequired,
}
pub(crate) struct Completion {
    pub state: CompletionState,
    pub config: Option<ConfigStore>,
    pub preferences: Option<Arc<CommandPreferencesSnapshot>>,
    pub report: Option<String>,
    pub based_on: ConfigRevision,
    pub setting_edit: Option<crikey_ui::SettingEditOutcome>,
    pub command_edit: Option<crikey_ui::CommandPreferenceEditOutcome>,
}
pub(crate) struct PreferenceWorker {
    sender: Option<mpsc::SyncSender<Request>>,
    receiver: mpsc::Receiver<Completion>,
    worker: Option<JoinHandle<()>>,
    settings: Option<mpsc::Receiver<SettingsResponse>>,
    closing: Arc<std::sync::atomic::AtomicBool>,
}
impl PreferenceWorker {
    pub fn spawn(
        store: Arc<Mutex<CommandPreferencesStore>>,
        initial: Option<ConfigStore>,
        directories: StandardDirectories,
        session_overrides: Vec<(String, String)>,
        policy: Option<PathBuf>,
        roots: Vec<PathBuf>,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self, String> {
        let (sender, requests) = mpsc::sync_channel(16);
        let (results, receiver) = mpsc::sync_channel(32);
        let (settings_results, settings) = mpsc::sync_channel(16);
        let closing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_closing = Arc::clone(&closing);
        let worker = thread::Builder::new()
            .name("configuration-edits".into())
            .spawn(move || {
                let mut state = WorkerConfiguration {
                    watch: initial.as_ref().map(ConfigStore::source_watch),
                    config: initial,
                    authoritative: false,
                    directories,
                    session_overrides,
                    settings: SettingsAuthority { policy, roots, ..Default::default() },
                };
                let startup = state.reload(&store);
                if results.send(startup).is_err() {
                    return;
                }
                wake();
                while !worker_closing.load(std::sync::atomic::Ordering::Acquire) {
                    let request = match requests.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(request) => request,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let completion = match request {
                        Request::Settings(request) => {
                            let mut completion = state.reload(&store);
                            let mut result = state.settings_request(&request);
                            let saved = matches!(&result, Ok(SettingsPayload::Saved { .. }));
                            if saved {
                                let preferences = store.lock().unwrap_or_else(|p| p.into_inner())
                                    .reload(state.config.as_ref().expect("authoritative store"));
                                match preferences {
                                    Ok(preferences) => completion = state.completion(completion.based_on.clone(), Some(preferences), None),
                                    Err(_) => result = Err(SettingsFailure::PersistedReload),
                                }
                            }
                            if matches!(&result, Err(SettingsFailure::PersistedReload)) {
                                completion = state.persisted_reload(completion.based_on.clone());
                            }
                            let response = SettingsResponse {
                                view: request.view,
                                request_id: request.request_id,
                                config_revision: state.settings.config_revision,
                                schema_revision: state.settings.schema_revision,
                                result,
                            };
                            if settings_results.send(response).is_err() { break; }
                            completion
                        }
                        Request::Package { change, reply } => {
                            let mut completion = state.reload(&store);
                            let result = if state.authoritative && state.settings.config_revision < u64::MAX {
                                if let Some((plugin, enabled)) = change {
                                    let based_on = completion.based_on.clone();
                                    match state.config.as_mut().expect("authoritative store").commit_plugin_enabled(&plugin, enabled) {
                                        Ok(()) => {
                                            let config = state.config.take().expect("authoritative store");
                                            state.replace(config);
                                            let preferences = store.lock().unwrap_or_else(|p| p.into_inner())
                                                .reload(state.config.as_ref().expect("authoritative store"));
                                            match preferences {
                                                Ok(preferences) => {
                                                    completion = state.completion(based_on, Some(preferences), None);
                                                    Ok(state.config.as_ref().expect("authoritative store").clone())
                                                }
                                                Err(_) => {
                                                    completion = state.persisted_reload(based_on);
                                                    Err(JobError::new("configuration-persisted-reload", "Configuration was saved but effective publication failed"))
                                                }
                                            }
                                        }
                                        Err(ConfigError::PersistedReload) => {
                                            completion = state.persisted_reload(based_on);
                                            Err(JobError::new("configuration-persisted-reload", "Configuration was saved but effective reload failed"))
                                        }
                                        Err(_) => Err(JobError::new("configuration-save", "Configuration could not be saved")),
                                    }
                                } else { Ok(state.config.as_ref().expect("authoritative store").clone()) }
                            } else {
                                Err(JobError::new("configuration-unavailable", "Configuration authority is unavailable"))
                            };
                            let _ = reply.send(result);
                            completion
                        }
                        Request::Poll => {
                            if state.authoritative
                                && state.watch.as_ref().is_some_and(|watch| !watch.changed())
                                && state.settings.roots.is_empty()
                            {
                                continue;
                            }
                            state.reload(&store)
                        }
                        Request::Mutate(MutationRequest {
                            request_id,
                            mut config,
                            registry,
                            expected_revision,
                            reference,
                            edit,
                            session,
                        }) => {
                            let based_on = state.revision();
                            let mut saved = false;
                            let submitted_reference = reference.clone();
                            let submitted_edit = edit.clone();
                            let mut completion = if !state.accepts(&config) {
                                state.conflict(&store)
                            } else {
                                let target = if session {
                                    ConfigLayer::SessionOverride
                                } else {
                                    ConfigLayer::UserGlobal
                                };
                                let result = config
                                    .command_preferences_at(target)
                                    .map_err(PreferenceError::Invalid)
                                    .and_then(|preferences| {
                                        mutation(&preferences.unwrap_or_default(), reference, edit)
                                            .map_err(PreferenceError::Invalid)
                                    })
                                    .and_then(|mutation| {
                                        store
                                            .lock()
                                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                                            .apply_snapshot(
                                                &mut config,
                                                &registry,
                                                expected_revision,
                                                mutation,
                                                session,
                                            )
                                    });
                                match result {
                                    Ok(preferences) => {
                                        let report = format!(
                                            "Command preferences saved; effective layer: {}",
                                            preferences.layer.map_or("unset", |layer| layer.as_str())
                                        );
                                        state.replace(config);
                                        saved = true;
                                        state.completion(based_on, Some(preferences), Some(report))
                                    }
                                    Err(PreferenceError::PersistedReload) => state.persisted_reload(based_on),
                                    Err(error) => state.completion(based_on, None, Some(describe(error))),
                                }
                            };
                            completion.command_edit = Some(crikey_ui::CommandPreferenceEditOutcome {
                                request_id,
                                reference: submitted_reference,
                                edit: submitted_edit,
                                status: edit_status(saved, completion.state),
                            });
                            completion
                        }
                        Request::Setting {
                            mut config,
                            request_id,
                            key,
                            value,
                        } => {
                            let based_on = state.revision();
                            let mut saved = false;
                            let mut completion = if !state.accepts(&config) {
                                state.conflict(&store)
                            } else if validate_setting_edit(&config, &key, &value).is_err() {
                                state.completion(based_on, None, Some("Setting edit is invalid".into()))
                            } else {
                                match crate::settings::persist_typed(&mut config, &key, &value) {
                                    Ok(report) => {
                                        let result = store
                                            .lock()
                                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                                            .reload(&config);
                                        state.replace(config);
                                        match result {
                                            Ok(preferences) => {
                                                saved = true;
                                                state.completion(based_on, Some(preferences), Some(report))
                                            }
                                            Err(_) => state.persisted_reload(based_on),
                                        }
                                    }
                                    Err(crikey_config::ConfigError::PersistedReload) => {
                                        state.persisted_reload(based_on)
                                    }
                                    Err(_) => state.completion(based_on, None, Some("Configuration could not be saved".into())),
                                }
                            };
                            completion.setting_edit = Some(crikey_ui::SettingEditOutcome {
                                request_id,
                                key,
                                value,
                                status: edit_status(saved, completion.state),
                            });
                            completion
                        }
                    };
                    if results.send(completion).is_err() {
                        break;
                    }
                    wake();
                }
            })
            .map_err(|_| "configuration edit worker could not start".to_owned())?;
        Ok(Self {
            sender: Some(sender),
            receiver,
            worker: Some(worker),
            settings: Some(settings),
            closing,
        })
    }
    /// Transfer settlement off the UI thread; continue draining both reply
    /// channels while the returned handle is joined by the shutdown owner.
    pub fn take_shutdown(&mut self) -> Option<JoinHandle<()>> {
        self.closing.store(true, std::sync::atomic::Ordering::Release);
        self.sender.take();
        self.worker.take()
    }
    pub fn package_configuration(&self) -> Arc<dyn PackageConfiguration> {
        Arc::new(PackageConfigurationClient {
            sender: self.sender.as_ref().expect("live worker").clone(),
        })
    }
    pub fn submit_settings(&self, request: SettingsRequest) -> Result<(), String> {
        let bounded = match &request.operation {
            SettingsOperation::Search { text } => text.len() <= 4096,
            SettingsOperation::BrowsePlugin { plugin } => plugin.0.len() <= 1024,
            SettingsOperation::Edit { key, draft, target } => {
                key.len() <= 4096
                    && draft.len() <= 4096
                    && match target {
                        SettingsTarget::UserGlobal => true,
                        SettingsTarget::PluginUser { plugin } => plugin.0.len() <= 1024,
                    }
            }
        };
        if !bounded {
            return Err("Settings request exceeds its limit".into());
        }
        self.submit(Request::Settings(request))
    }
    pub fn take_settings(&self) -> Option<SettingsResponse> {
        self.settings.as_ref()?.try_recv().ok()
    }
    pub fn submit(&self, request: Request) -> Result<(), String> {
        self.try_submit(request).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => "Configuration edit queue is busy; retry this edit".to_owned(),
            mpsc::TrySendError::Disconnected(_) => "Configuration edits are closed".to_owned(),
        })
    }
    fn try_submit(&self, request: Request) -> Result<(), mpsc::TrySendError<Request>> {
        match self.sender.as_ref() {
            Some(sender) => sender.try_send(request),
            None => Err(mpsc::TrySendError::Disconnected(request)),
        }
    }

    /// Failed admission returns the original submitted identity and draft.
    pub fn submit_setting(
        &self,
        request_id: u64,
        config: ConfigStore,
        key: String,
        value: String,
    ) -> Result<(), (crikey_ui::SettingEditOutcome, String)> {
        self.try_submit(Request::Setting {
            config,
            request_id,
            key,
            value,
        })
        .map_err(|error| {
            let (request, message) = match error {
                mpsc::TrySendError::Full(request) => {
                    (request, "Configuration edit queue is busy; retry this edit")
                }
                mpsc::TrySendError::Disconnected(request) => (request, "Configuration edits are closed"),
            };
            let Request::Setting {
                request_id,
                key,
                value,
                ..
            } = request
            else {
                unreachable!()
            };
            (
                crikey_ui::SettingEditOutcome {
                    request_id,
                    key,
                    value,
                    status: crikey_ui::SettingEditStatus::Failed,
                },
                message.to_owned(),
            )
        })
    }
    pub fn submit_mutation(
        &self,
        request: MutationRequest,
    ) -> Result<(), (crikey_ui::CommandPreferenceEditOutcome, String)> {
        self.try_submit(Request::Mutate(request)).map_err(|error| {
            let (request, message) = match error {
                mpsc::TrySendError::Full(request) => {
                    (request, "Configuration edit queue is busy; retry this edit")
                }
                mpsc::TrySendError::Disconnected(request) => (request, "Configuration edits are closed"),
            };
            let Request::Mutate(MutationRequest {
                request_id,
                reference,
                edit,
                ..
            }) = request
            else {
                unreachable!()
            };
            (
                crikey_ui::CommandPreferenceEditOutcome {
                    request_id,
                    reference,
                    edit,
                    status: crikey_ui::SettingEditStatus::Failed,
                },
                message.to_owned(),
            )
        })
    }
    pub fn take(&self) -> Option<Completion> {
        self.receiver.try_recv().ok()
    }
}
impl Drop for PreferenceWorker {
    fn drop(&mut self) {
        self.closing.store(true, std::sync::atomic::Ordering::Release);
        self.settings.take();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            while self.receiver.recv().is_ok() {}
            let _ = worker.join();
        }
    }
}
/// Only this serial worker observes sources and advances canonical configuration.
struct WorkerConfiguration {
    config: Option<ConfigStore>,
    watch: Option<ConfigSourceWatch>,
    authoritative: bool,
    directories: StandardDirectories,
    session_overrides: Vec<(String, String)>,
    settings: SettingsAuthority,
}
impl WorkerConfiguration {
    fn settings_row(
        &self,
        key: &str,
        requires_restart: bool,
        secret: bool,
    ) -> Result<SettingsRow, SettingsFailure> {
        let config = self.config.as_ref().ok_or(SettingsFailure::Unavailable)?;
        let value = if secret {
            ""
        } else {
            launcher_settings::declaration(key)
                .map(|declaration| declaration.read(Some(config)).value)
                .unwrap_or_else(|| config.display_value(key).unwrap_or(""))
        };
        if key.len() > 4096 {
            return Err(SettingsFailure::Invalid);
        }
        let value_available = secret || value.len() <= 8192 - key.len();
        Ok(SettingsRow {
            key: key.to_owned(),
            value: if value_available {
                value.to_owned()
            } else {
                String::new()
            },
            source: launcher_settings::declaration(key)
                .map(|declaration| declaration.read(Some(config)).source)
                .unwrap_or_else(|| config.layer_of(key).map_or("unset", |layer| layer.as_str()))
                .to_owned(),
            requires_restart,
            secret,
            value_available,
        })
    }

    fn settings_request(&mut self, request: &SettingsRequest) -> Result<SettingsPayload, SettingsFailure> {
        if !self.authoritative {
            return Err(SettingsFailure::Unavailable);
        }
        if request.expected_schema != 0 && request.expected_schema != self.settings.schema_revision {
            return Err(SettingsFailure::StaleSchema);
        }
        match &request.operation {
            SettingsOperation::Search { text } => {
                let definitions = launcher_settings::search_definitions();
                let index = SettingsSearch::new(
                    self.settings.schema_revision,
                    &definitions,
                    &self.settings.schemas,
                    std::env::consts::OS,
                )
                .map_err(|_| SettingsFailure::Unavailable)?;
                let mut results = index
                    .search_owned(text, self.settings.schema_revision)
                    .map_err(|_| SettingsFailure::Invalid)?;
                let mut rows = Vec::with_capacity(results.hits.len());
                let mut bytes = 1024
                    + results.hits.capacity()
                        * std::mem::size_of::<crikey_app::settings_search::OwnedSettingMatch>()
                    + rows.capacity() * std::mem::size_of::<SettingsRow>();
                for hit in &results.hits {
                    let row = self.settings_row(&hit.key, hit.requires_restart, false)?;
                    bytes += row.key.capacity()
                        + row.value.capacity()
                        + row.source.capacity()
                        + hit.key.capacity()
                        + hit.label.capacity()
                        + hit.description.capacity()
                        + hit.category.capacity()
                        + hit.navigation_target.capacity()
                        + hit.control.capacity();
                    if bytes > 60 * 1024 {
                        break;
                    }
                    rows.push(row);
                }
                if rows.len() < results.hits.len() {
                    results.truncated = true;
                    results.hits.truncate(rows.len());
                }
                Ok(SettingsPayload::Search { results, rows })
            }
            SettingsOperation::BrowsePlugin { plugin } => {
                let owner = self
                    .settings
                    .schemas
                    .iter()
                    .find(|owner| &owner.plugin == plugin)
                    .ok_or(SettingsFailure::Unavailable)?;
                let capacity = owner.section.fields.len().min(128);
                let mut results = SettingsSearchResults {
                    schema_revision: self.settings.schema_revision,
                    hits: Vec::with_capacity(capacity),
                    truncated: false,
                };
                let mut rows = Vec::with_capacity(capacity);
                let mut bytes = 1024
                    + results.hits.capacity()
                        * std::mem::size_of::<crikey_app::settings_search::OwnedSettingMatch>()
                    + rows.capacity() * std::mem::size_of::<SettingsRow>();
                for field in owner
                    .section
                    .fields
                    .iter()
                    .filter(|field| field.applies_to(std::env::consts::OS))
                {
                    let key = format!("plugins.{}.settings.{}", plugin.0, field.name);
                    let row = self.settings_row(&key, field.requires_restart, field.secret)?;
                    bytes += key.len() * 2
                        + row.value.len()
                        + row.source.len()
                        + field.name.len()
                        + field.description.len()
                        + plugin.0.len()
                        + field.kind.as_str().len()
                        + 512;
                    if rows.len() == 128 || bytes > 60 * 1024 {
                        results.truncated = true;
                        break;
                    }
                    results.hits.push(crikey_app::settings_search::OwnedSettingMatch {
                        key,
                        label: field.name.clone(),
                        description: field.description.clone(),
                        category: "Plugins".into(),
                        navigation_target: plugin.0.clone(),
                        control: field.kind.as_str().into(),
                        requires_restart: field.requires_restart,
                    });
                    rows.push(row);
                }
                Ok(SettingsPayload::Search { results, rows })
            }
            SettingsOperation::Edit { key, target, draft } => {
                if key.len() > 4096 || draft.len() > 4096 {
                    return Err(SettingsFailure::Invalid);
                }
                if request.expected_schema != self.settings.schema_revision {
                    return Err(SettingsFailure::StaleSchema);
                }
                if request.expected_config != self.settings.config_revision {
                    return Err(SettingsFailure::Conflict);
                }
                if self.settings.config_revision == u64::MAX {
                    return Err(SettingsFailure::Unavailable);
                }
                let config = self.config.as_mut().ok_or(SettingsFailure::Unavailable)?;
                let expected = config.revision();
                let (result, restart, secret) = match target {
                    SettingsTarget::UserGlobal => {
                        let declaration =
                            launcher_settings::declaration(key).ok_or(SettingsFailure::Invalid)?;
                        validate_setting_edit(config, key, draft).map_err(|_| SettingsFailure::Invalid)?;
                        (
                            config.commit_user_global(key, draft, &expected),
                            declaration.restart == launcher_settings::Restart::Required,
                            false,
                        )
                    }
                    SettingsTarget::PluginUser { plugin } => {
                        let owner = self
                            .settings
                            .schemas
                            .iter()
                            .find(|owner| &owner.plugin == plugin)
                            .ok_or(SettingsFailure::Unavailable)?;
                        let prefix = format!("plugins.{}.settings.", plugin.0);
                        let name = key.strip_prefix(&prefix).ok_or(SettingsFailure::Invalid)?;
                        let field = owner
                            .section
                            .fields
                            .iter()
                            .find(|field| field.name == name && field.applies_to(std::env::consts::OS))
                            .ok_or(SettingsFailure::Unavailable)?;
                        (
                            config.commit_plugin_user(owner, name, draft, &expected),
                            field.requires_restart,
                            field.secret,
                        )
                    }
                };
                match result {
                    Ok(()) => {
                        let config = self.config.take().ok_or(SettingsFailure::Unavailable)?;
                        self.replace(config);
                        if !self.authoritative {
                            return Err(SettingsFailure::Unavailable);
                        }
                        Ok(SettingsPayload::Saved {
                            row: self.settings_row(key, restart, secret)?,
                        })
                    }
                    Err(ConfigError::Conflict) => Err(SettingsFailure::Conflict),
                    Err(ConfigError::PersistedReload) => {
                        self.authoritative = false;
                        Err(SettingsFailure::PersistedReload)
                    }
                    Err(_) => Err(SettingsFailure::Invalid),
                }
            }
        }
    }
    fn revision(&self) -> ConfigRevision {
        self.config
            .as_ref()
            .map(ConfigStore::revision)
            .unwrap_or_default()
    }
    fn accepts(&mut self, draft: &ConfigStore) -> bool {
        if self.watch.as_ref().is_some_and(ConfigSourceWatch::changed) {
            self.authoritative = false;
        }
        self.authoritative
            && self.settings.config_revision < u64::MAX
            && self
                .config
                .as_ref()
                .is_some_and(|current| current.revision() == draft.revision())
    }
    fn replace(&mut self, config: ConfigStore) {
        if self.settings.config_revision == 0
            || self
                .config
                .as_ref()
                .is_none_or(|old| old.revision() != config.revision())
        {
            let Some(next) = self.settings.config_revision.checked_add(1) else {
                self.authoritative = false;
                return;
            };
            self.settings.config_revision = next;
        }
        self.watch = Some(config.source_watch());
        self.config = Some(config);
        self.authoritative = true;
    }
    fn completion(
        &self,
        based_on: ConfigRevision,
        preferences: Option<Arc<CommandPreferencesSnapshot>>,
        report: Option<String>,
    ) -> Completion {
        Completion {
            state: if self.authoritative {
                CompletionState::Authoritative
            } else {
                CompletionState::Unavailable
            },
            config: self.config.clone(),
            preferences,
            report,
            based_on,
            setting_edit: None,
            command_edit: None,
        }
    }
    fn persisted_reload(&mut self, based_on: ConfigRevision) -> Completion {
        self.authoritative = false;
        let mut completion =
            self.completion(based_on, None, Some(describe(PreferenceError::PersistedReload)));
        completion.state = CompletionState::PersistedReloadRequired;
        completion
    }
    fn conflict(&self, store: &Mutex<CommandPreferencesStore>) -> Completion {
        let preferences = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot();
        let report = if self.authoritative {
            "Configuration changed; reload and retry this edit"
        } else {
            "Configuration is unavailable; edits are disabled until it reloads successfully"
        };
        self.completion(self.revision(), Some(preferences), Some(report.into()))
    }
    fn reload(&mut self, store: &Mutex<CommandPreferencesStore>) -> Completion {
        let based_on = self.revision();
        self.authoritative = false;
        let loaded = match &self.config {
            Some(current) => current.reload_unregistered(),
            None => ConfigStore::load_with_overrides(
                &self.directories,
                self.settings.policy.as_deref(),
                &self.session_overrides,
            ),
        };
        let mut config = match loaded {
            Ok(config) => config,
            Err(_) => {
                return self.completion(based_on, None, Some("Configuration could not be reloaded".into()))
            }
        };
        let (schemas, problems) = crikey_config::discover_plugin_schemas(&self.settings.roots);
        if !problems.is_empty() {
            return self.completion(
                based_on,
                None,
                Some("Plugin configuration declarations are unavailable".into()),
            );
        }
        let problems = config.replace_discovered_schemas(&schemas);
        if problems
            .iter()
            .any(|problem| !matches!(problem, ConfigError::Schema { .. }))
        {
            return self.completion(
                based_on,
                None,
                Some("Plugin configuration declaration changed or is unavailable".into()),
            );
        }
        if self.settings.schema_revision == 0
            || self
                .config
                .as_ref()
                .is_none_or(|previous| !config.same_discovered_schemas(previous))
        {
            let Some(next) = self.settings.schema_revision.checked_add(1) else {
                return self.completion(
                    based_on,
                    None,
                    Some("Settings schema revision is exhausted".into()),
                );
            };
            self.settings.schema_revision = next;
        }
        self.settings.schemas = schemas;
        let result = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reload(&config);
        match result {
            Ok(preferences) => {
                self.replace(config);
                self.completion(based_on, Some(preferences), None)
            }
            Err(error) => self.completion(based_on, None, Some(describe(error))),
        }
    }
}

fn describe(error: PreferenceError) -> String {
    match error {
        PreferenceError::Conflict { .. } => "Command preferences changed; reload and retry this edit".into(),
        PreferenceError::Invalid(_) => "Command preference edit is invalid".into(),
        PreferenceError::Persistence(_) => "Command preferences were not saved".into(),
        PreferenceError::PersistedReload => {
            "Command preferences were saved, but the effective configuration could not be reloaded".into()
        }
        PreferenceError::RevisionExhausted => "Command preference revision is exhausted; edit refused".into(),
    }
}

fn mutation(
    preferences: &crikey_config::command_preferences::CommandPreferences,
    reference: crikey_core::CommandRef,
    edit: crikey_ui::CommandPreferenceEdit,
) -> Result<PreferenceMutation, String> {
    use crikey_ui::CommandPreferenceEdit;
    match edit {
        CommandPreferenceEdit::SetAlias(alias) => Ok(PreferenceMutation::SetAlias { reference, alias }),
        CommandPreferenceEdit::SetHotkey(hotkey) => Ok(PreferenceMutation::SetHotkey { reference, hotkey }),
        CommandPreferenceEdit::Hidden(hidden) => {
            let mut record = preferences
                .records
                .iter()
                .find(|record| record.reference == reference)
                .cloned()
                .unwrap_or(crikey_config::command_preferences::CommandPreference {
                    reference,
                    hidden: false,
                    alias: None,
                    hotkey: None,
                });
            record.hidden = hidden;
            Ok(PreferenceMutation::SetRecord(record))
        }
        CommandPreferenceEdit::Favorite(enabled) => {
            let mut list = preferences.favorites.clone();
            if enabled {
                if !list.contains(&reference) {
                    list.push(reference);
                }
            } else {
                list.retain(|saved| saved != &reference);
            }
            Ok(PreferenceMutation::SetFavorites(list))
        }
        CommandPreferenceEdit::Fallback(enabled) => {
            let mut list = preferences.fallbacks.clone();
            if enabled {
                if !list.contains(&reference) {
                    list.push(reference);
                }
            } else {
                list.retain(|saved| saved != &reference);
            }
            Ok(PreferenceMutation::SetFallbacks(list))
        }
        CommandPreferenceEdit::MoveFavorite { position } => {
            let mut list = preferences.favorites.clone();
            move_reference(&mut list, &reference, position)?;
            Ok(PreferenceMutation::SetFavorites(list))
        }
        CommandPreferenceEdit::MoveFallback { position } => {
            let mut list = preferences.fallbacks.clone();
            move_reference(&mut list, &reference, position)?;
            Ok(PreferenceMutation::SetFallbacks(list))
        }
    }
}
fn move_reference(
    list: &mut Vec<crikey_core::CommandRef>,
    reference: &crikey_core::CommandRef,
    position: usize,
) -> Result<(), String> {
    if position >= list.len() {
        return Err("Saved command position is no longer current".into());
    }
    let old = list
        .iter()
        .position(|saved| saved == reference)
        .ok_or("Saved command is no longer in this list")?;
    let reference = list.remove(old);
    list.insert(position, reference);
    Ok(())
}

fn edit_status(saved: bool, state: CompletionState) -> crikey_ui::SettingEditStatus {
    if state == CompletionState::PersistedReloadRequired {
        crikey_ui::SettingEditStatus::PersistedReloadRequired
    } else if saved {
        crikey_ui::SettingEditStatus::Saved
    } else {
        crikey_ui::SettingEditStatus::Failed
    }
}

fn validate_setting_edit(config: &ConfigStore, key: &str, value: &str) -> Result<(), String> {
    launcher_settings::validate(key, value)?;
    if key == crikey_config::KEY_ACTIVATION_HOTKEY {
        let (current, retained) = config
            .project_user_global_candidates(key, value)
            .map_err(|_| "Configuration could not be projected".to_owned())?;
        current.validate_command_hotkey_conflicts()?;
        retained.validate_command_hotkey_conflicts()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_config::command_preferences::{CommandPreference, CommandPreferences};
    use crikey_core::{CommandRef, PluginId};
    use crikey_ui::CommandPreferenceEdit;

    fn reference(owner: &str, command: &str) -> CommandRef {
        CommandRef {
            provider_id: PluginId(owner.into()),
            command_id: command.into(),
        }
    }

    fn snapshot() -> CommandPreferencesSnapshot {
        let selected = reference("owner.one", "shared-command");
        let other = reference("owner.two", "shared-command");
        CommandPreferencesSnapshot {
            revision: 7,
            layer: Some(ConfigLayer::UserGlobal),
            preferences: CommandPreferences {
                records: vec![CommandPreference {
                    reference: selected.clone(),
                    hidden: false,
                    alias: Some("saved-alias".into()),
                    hotkey: Some("Ctrl+Shift+J".into()),
                }],
                favorites: vec![selected.clone(), other.clone()],
                fallbacks: vec![other, selected],
            },
        }
    }

    #[test]
    fn hidden_toggle_preserves_record_fields_and_memberships() {
        let mut snapshot = snapshot();
        let favorites = snapshot.preferences.favorites.clone();
        let fallbacks = snapshot.preferences.fallbacks.clone();
        let original = snapshot.preferences.records[0].clone();
        for hidden in [true, false] {
            let edit = mutation(
                &snapshot.preferences,
                original.reference.clone(),
                CommandPreferenceEdit::Hidden(hidden),
            )
            .unwrap();
            let PreferenceMutation::SetRecord(record) = edit else {
                panic!("visibility edits must only replace the exact preference record");
            };
            assert_eq!(record.reference, original.reference);
            assert_eq!(record.hidden, hidden);
            assert_eq!(record.alias, original.alias);
            assert_eq!(record.hotkey, original.hotkey);
            snapshot.preferences.records[0] = record;
            assert_eq!(snapshot.preferences.favorites, favorites);
            assert_eq!(snapshot.preferences.fallbacks, fallbacks);
        }
    }

    #[test]
    fn removing_saved_membership_does_not_remove_the_preference_record() {
        let snapshot = snapshot();
        let selected = snapshot.preferences.records[0].reference.clone();
        let other = reference("owner.two", "shared-command");
        let records = snapshot.preferences.records.clone();
        let favorite = mutation(
            &snapshot.preferences,
            selected.clone(),
            CommandPreferenceEdit::Favorite(false),
        )
        .unwrap();
        let PreferenceMutation::SetFavorites(favorites) = favorite else {
            panic!("removing a favorite must only change favorite membership");
        };
        assert_eq!(favorites, vec![other.clone()]);
        let fallback = mutation(
            &snapshot.preferences,
            selected,
            CommandPreferenceEdit::Fallback(false),
        )
        .unwrap();
        let PreferenceMutation::SetFallbacks(fallbacks) = fallback else {
            panic!("removing a fallback must only change fallback membership");
        };
        assert_eq!(fallbacks, vec![other]);
        assert_eq!(snapshot.preferences.records, records);
    }

    #[test]
    fn moving_saved_commands_preserves_exact_owner_qualified_identity() {
        let snapshot = snapshot();
        let selected = snapshot.preferences.records[0].reference.clone();
        let other = reference("owner.two", "shared-command");
        let favorite = mutation(
            &snapshot.preferences,
            selected.clone(),
            CommandPreferenceEdit::MoveFavorite { position: 1 },
        )
        .unwrap();
        let PreferenceMutation::SetFavorites(favorites) = favorite else {
            panic!("moving a favorite must only reorder favorites");
        };
        assert_eq!(favorites, vec![other.clone(), selected.clone()]);
        let fallback = mutation(
            &snapshot.preferences,
            selected.clone(),
            CommandPreferenceEdit::MoveFallback { position: 0 },
        )
        .unwrap();
        let PreferenceMutation::SetFallbacks(fallbacks) = fallback else {
            panic!("moving a fallback must only reorder fallbacks");
        };
        assert_eq!(fallbacks, vec![selected, other]);
    }

    #[test]
    fn moving_saved_commands_rejects_stale_positions_and_missing_references() {
        let snapshot = snapshot();
        let selected = snapshot.preferences.records[0].reference.clone();
        for edit in [
            CommandPreferenceEdit::MoveFavorite {
                position: snapshot.preferences.favorites.len(),
            },
            CommandPreferenceEdit::MoveFallback {
                position: snapshot.preferences.fallbacks.len(),
            },
        ] {
            assert_eq!(
                mutation(&snapshot.preferences, selected.clone(), edit).unwrap_err(),
                "Saved command position is no longer current"
            );
        }
        let absent = reference("owner.absent", "shared-command");
        for edit in [
            CommandPreferenceEdit::MoveFavorite { position: 0 },
            CommandPreferenceEdit::MoveFallback { position: 0 },
        ] {
            assert_eq!(
                mutation(&snapshot.preferences, absent.clone(), edit).unwrap_err(),
                "Saved command is no longer in this list"
            );
        }
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;
    use crikey_platform::{DirectoryConvention, DirectoryEnvironment};
    use std::path::PathBuf;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("crikey-preference-authority-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn external_edit_then_package_transaction_publishes_from_consumers_accepted_revision() {
        use std::time::{Duration, Instant};
        let scratch = Scratch::new();
        let directories = StandardDirectories::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::new()
                .set("HOME", &scratch.0)
                .set("APPDATA", &scratch.0)
                .set("LOCALAPPDATA", &scratch.0)
                .set("CRIKEY_CONFIG_DIR", &scratch.0),
        )
        .unwrap();
        let initial = ConfigStore::load_with_policy(&directories, None).unwrap();
        let path = initial.config_path().to_owned();
        std::fs::write(&path, "[plugins.modern.fixture.settings]\nstatus = \"before\"\n").unwrap();
        let initial = initial.reload().unwrap();
        let worker = PreferenceWorker::spawn(
            Arc::new(Mutex::new(CommandPreferencesStore::empty())),
            Some(initial),
            directories,
            Vec::new(),
            None,
            Vec::new(),
            || {},
        )
        .unwrap();
        let startup = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut consumer = crate::LauncherConfiguration::from_store(startup.config.unwrap());
        let plugin = PluginId("modern.fixture".into());
        let now = Instant::now();
        consumer.seed(now);
        let before = consumer.publisher.poll(now + Duration::from_secs(2)).unwrap();
        assert_eq!(
            before
                .values_for(&plugin)
                .unwrap()
                .get("status")
                .map(String::as_str),
            Some("before")
        );

        let client = worker.package_configuration();
        std::fs::write(
            &path,
            "[plugins.modern.fixture.settings]\nstatus = \"external\"\n",
        )
        .unwrap();
        let effective = client.set_plugin_enabled(&plugin, true).unwrap();
        let completion = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        // The composition owner still has A, not the worker's intermediate
        // fresh external read B. Only an A-based completion can publish C.
        assert_eq!(completion.based_on, consumer.store.revision());
        assert_ne!(effective.revision(), consumer.store.revision());
        assert_eq!(
            completion.config.as_ref().unwrap().revision(),
            effective.revision()
        );
        consumer.store = completion.config.unwrap();
        assert_eq!(consumer.store.get("plugins.modern.fixture.enabled"), Some("true"));
        consumer.seed(now + Duration::from_secs(3));
        let published = consumer.publisher.poll(now + Duration::from_secs(5)).unwrap();
        assert_eq!(
            published
                .values_for(&plugin)
                .unwrap()
                .get("status")
                .map(String::as_str),
            Some("external")
        );

        // A read-only package admission reload also owes its external edge.
        std::fs::write(&path, "[plugins.modern.fixture]\nenabled = true\n[plugins.modern.fixture.settings]\nstatus = \"load-only\"\n").unwrap();
        let loaded = client.load().unwrap();
        let completion = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(completion.based_on, consumer.store.revision());
        assert_eq!(completion.config.as_ref().unwrap().revision(), loaded.revision());
        consumer.store = completion.config.unwrap();
        consumer.seed(now + Duration::from_secs(6));
        let published = consumer.publisher.poll(now + Duration::from_secs(8)).unwrap();
        assert_eq!(
            published
                .values_for(&plugin)
                .unwrap()
                .get("status")
                .map(String::as_str),
            Some("load-only")
        );

        client.set_plugin_enabled(&plugin, false).unwrap();
        let next = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(next.based_on, consumer.store.revision());
        assert_eq!(
            next.config.unwrap().get("plugins.modern.fixture.enabled"),
            Some("false")
        );
    }

    #[test]
    fn saved_plugin_edit_publishes_when_large_session_value_cannot_be_displayed() {
        use std::time::{Duration, Instant};
        let scratch = Scratch::new();
        let directories = StandardDirectories::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::new()
                .set("HOME", &scratch.0)
                .set("APPDATA", &scratch.0)
                .set("LOCALAPPDATA", &scratch.0)
                .set("CRIKEY_CONFIG_DIR", &scratch.0),
        )
        .unwrap();
        let root = scratch.0.join("packages");
        let package = root.join("fixture");
        std::fs::create_dir_all(&package).unwrap();
        let manifest = concat!(
            include_str!("../../crikey-plugin-model/tests/data/spec-example.crikey.toml"),
            "\n[[configuration.field]]\nname = \"visible\"\ntype = \"string\"\n",
        );
        std::fs::write(package.join("crikey.toml"), manifest).unwrap();
        let plugin = PluginId("native.dev.example.repositories".into());
        let key = format!("plugins.{}.settings.visible", plugin.0);
        let large = "x".repeat(9000);
        let overrides = vec![(key.clone(), large.clone())];
        let config = ConfigStore::load_with_overrides(&directories, None, &overrides).unwrap();
        let worker = PreferenceWorker::spawn(
            Arc::new(Mutex::new(CommandPreferencesStore::empty())),
            Some(config),
            directories.clone(),
            overrides,
            None,
            vec![root],
            || {},
        )
        .unwrap();
        let startup = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut consumer = crate::LauncherConfiguration::from_store(startup.config.unwrap());
        let mut model = crikey_ui::LauncherViewModel::new();
        model.activate();
        let view = model.view().unwrap().0;
        worker
            .submit_settings(SettingsRequest {
                view,
                request_id: 1,
                expected_config: 0,
                expected_schema: 0,
                operation: SettingsOperation::BrowsePlugin {
                    plugin: plugin.clone(),
                },
            })
            .unwrap();
        let browse = worker
            .settings
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        let SettingsPayload::Search { rows, .. } = browse.result.unwrap() else {
            panic!("browse")
        };
        assert!(!rows[0].value_available);
        assert!(rows[0].value.is_empty());
        let refresh = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(refresh.based_on, consumer.store.revision());
        consumer.store = refresh.config.unwrap();

        worker
            .submit_settings(SettingsRequest {
                view,
                request_id: 2,
                expected_config: browse.config_revision,
                expected_schema: browse.schema_revision,
                operation: SettingsOperation::Edit {
                    key: key.clone(),
                    target: SettingsTarget::PluginUser {
                        plugin: plugin.clone(),
                    },
                    draft: "persisted".into(),
                },
            })
            .unwrap();
        let response = worker
            .settings
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(response.view, view);
        assert_eq!(response.request_id, 2);
        let SettingsPayload::Saved { row } = response.result.unwrap() else {
            panic!("saved acknowledgement")
        };
        assert!(!row.value_available);
        assert!(row.value.is_empty());
        let completion = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(completion.state, CompletionState::Authoritative);
        assert_eq!(completion.based_on, consumer.store.revision());
        let saved = completion.config.unwrap();
        assert_ne!(saved.revision(), consumer.store.revision());
        assert_eq!(saved.get(&key), Some(large.as_str()));
        consumer.store = saved;
        let now = Instant::now();
        consumer.seed(now);
        let published = consumer.publisher.poll(now + Duration::from_secs(2)).unwrap();
        assert_eq!(
            published
                .values_for(&plugin)
                .unwrap()
                .get("visible")
                .map(String::as_str),
            Some(large.as_str())
        );
        let persisted = ConfigStore::load_with_policy(&directories, None).unwrap();
        assert_eq!(persisted.get(&key), Some("persisted"));
    }

    #[test]
    fn owned_settings_survive_reload_and_keep_secret_drafts_out_of_projection() {
        use crikey_plugin_model::{ConfigurationField, ConfigurationKind, ConfigurationSection};
        let scratch = Scratch::new();
        let directories = StandardDirectories::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::new()
                .set("HOME", &scratch.0)
                .set("APPDATA", &scratch.0)
                .set("LOCALAPPDATA", &scratch.0)
                .set("CRIKEY_CONFIG_DIR", &scratch.0),
        )
        .unwrap();
        let mut config = ConfigStore::load_with_policy(&directories, None).unwrap();
        let plugin = PluginId("modern.fixture".into());
        let owner = DiscoveredSchema {
            plugin: plugin.clone(),
            package: scratch.0.join("package"),
            section: ConfigurationSection {
                fields: vec![ConfigurationField {
                    name: "token".into(),
                    kind: ConfigurationKind::String,
                    default: None,
                    description: "Private token".into(),
                    secret: true,
                    requires_restart: true,
                    platforms: Vec::new(),
                    required: false,
                    minimum: None,
                    maximum: None,
                    max_length: None,
                    allowed: Vec::new(),
                }],
            },
        };
        assert!(config.register_discovered_schema(&owner).is_empty());
        config.set_session_override("plugins.modern.fixture.settings.token", "private-canary");
        let mut state = WorkerConfiguration {
            watch: Some(config.source_watch()),
            config: Some(config),
            authoritative: true,
            directories,
            session_overrides: Vec::new(),
            settings: SettingsAuthority {
                schemas: vec![owner],
                config_revision: 4,
                schema_revision: 7,
                ..Default::default()
            },
        };
        let mut model = crikey_ui::LauncherViewModel::new();
        model.activate();
        let view = model.view().unwrap().0;
        let mut request = SettingsRequest {
            view,
            request_id: 1,
            expected_config: 4,
            expected_schema: 7,
            operation: SettingsOperation::BrowsePlugin {
                plugin: plugin.clone(),
            },
        };
        let response = state.settings_request(&request).unwrap();
        let SettingsPayload::Search { results, rows } = response else {
            panic!("browse")
        };
        assert_eq!(results.hits[0].key, "plugins.modern.fixture.settings.token");
        assert!(rows[0].secret);
        assert!(rows[0].value.is_empty());
        state.settings.schemas.clear();
        state.settings.schema_revision = 8;
        assert_eq!(rows[0].key, "plugins.modern.fixture.settings.token");
        request.operation = SettingsOperation::Edit {
            key: rows[0].key.clone(),
            target: SettingsTarget::PluginUser { plugin },
            draft: "new-private-canary".into(),
        };
        assert!(matches!(
            state.settings_request(&request),
            Err(SettingsFailure::StaleSchema)
        ));
        request.expected_schema = 8;
        request.expected_config = 3;
        assert!(matches!(
            state.settings_request(&request),
            Err(SettingsFailure::Conflict)
        ));
        let SettingsOperation::Edit { draft, .. } = request.operation else {
            panic!("draft")
        };
        assert_eq!(draft, "new-private-canary");
    }

    #[test]
    fn failed_external_reload_revokes_edits_until_successful_recovery() {
        let scratch = Scratch::new();
        let directories = StandardDirectories::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::new()
                .set("HOME", &scratch.0)
                .set("APPDATA", &scratch.0)
                .set("LOCALAPPDATA", &scratch.0)
                .set("CRIKEY_CONFIG_DIR", &scratch.0),
        )
        .unwrap();
        let config = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
        let draft = config.clone();
        let path = config.config_path().to_owned();
        let store = Mutex::new(CommandPreferencesStore::load(&config).unwrap());
        let mut state = WorkerConfiguration {
            watch: Some(config.source_watch()),
            config: Some(config),
            authoritative: true,
            directories,
            session_overrides: Vec::new(),
            settings: SettingsAuthority::default(),
        };
        let published = store.lock().unwrap().snapshot();
        assert!(state.accepts(&draft));

        std::fs::write(&path, "[invalid").unwrap();
        // Even before Poll, a session-only mutation must not use stale authority.
        assert!(!state.accepts(&draft));
        let failed = state.reload(&store);
        assert!(failed.report.is_some());
        assert!(failed.preferences.is_none());
        assert_eq!(state.revision(), draft.revision());
        assert_eq!(store.lock().unwrap().snapshot().revision, published.revision);
        assert!(!state.accepts(&draft));

        std::fs::write(&path, "").unwrap();
        let recovered = state.reload(&store);
        assert!(recovered.report.is_none());
        assert!(recovered.preferences.is_some());
        let current = state.config.as_ref().unwrap().clone();
        assert!(state.accepts(&current));
        assert!(!state.accepts(&draft));
    }

    #[test]
    fn setting_acknowledgments_preserve_requested_drafts_on_failure_and_conflict() {
        use crikey_ui::{SettingEditOutcome, SettingEditStatus};
        use std::time::Duration;
        let scratch = Scratch::new();
        let directories = StandardDirectories::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::new()
                .set("HOME", &scratch.0)
                .set("APPDATA", &scratch.0)
                .set("LOCALAPPDATA", &scratch.0)
                .set("CRIKEY_CONFIG_DIR", &scratch.0),
        )
        .unwrap();
        let config = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
        let path = config.config_path().to_owned();
        let store = Arc::new(Mutex::new(CommandPreferencesStore::empty()));
        let worker = PreferenceWorker::spawn(
            store,
            Some(config),
            directories.clone(),
            Vec::new(),
            None,
            Vec::new(),
            || {},
        )
        .unwrap();
        let initial = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        let initial_config = initial.config.unwrap();
        let key = "launcher.command-home";

        worker
            .submit(Request::Setting {
                config: initial_config.clone(),
                request_id: 11,
                key: key.into(),
                value: "invalid-choice".into(),
            })
            .unwrap();
        let invalid = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(invalid.report.is_some());
        assert_eq!(
            invalid.setting_edit,
            Some(SettingEditOutcome {
                request_id: 11,
                key: key.into(),
                value: "invalid-choice".into(),
                status: SettingEditStatus::Failed,
            })
        );
        assert_eq!(
            invalid.config.as_ref().unwrap().revision(),
            initial_config.revision()
        );

        worker
            .submit(Request::Setting {
                config: initial_config.clone(),
                request_id: 12,
                key: key.into(),
                value: "favorites".into(),
            })
            .unwrap();
        let saved = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            saved.setting_edit,
            Some(SettingEditOutcome {
                request_id: 12,
                key: key.into(),
                value: "favorites".into(),
                status: SettingEditStatus::Saved,
            })
        );
        let current = saved.config.unwrap();
        assert_eq!(
            crate::LauncherConfiguration::load_store(&directories, &[])
                .unwrap()
                .get(key),
            Some("favorites")
        );

        // A queued edit based on the pre-save UI snapshot must acknowledge its
        // own requested value as failed, not clear a newer draft with request 12.
        worker
            .submit(Request::Setting {
                config: initial_config,
                request_id: 13,
                key: key.into(),
                value: "recent".into(),
            })
            .unwrap();
        let stale = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            stale.setting_edit,
            Some(SettingEditOutcome {
                request_id: 13,
                key: key.into(),
                value: "recent".into(),
                status: SettingEditStatus::Failed,
            })
        );
        assert_eq!(stale.config.as_ref().unwrap().get(key), Some("favorites"));

        std::fs::write(&path, "[invalid").unwrap();
        worker
            .submit(Request::Setting {
                config: current,
                request_id: 14,
                key: key.into(),
                value: "favorites-and-recent".into(),
            })
            .unwrap();
        let unavailable = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            unavailable.setting_edit,
            Some(SettingEditOutcome {
                request_id: 14,
                key: key.into(),
                value: "favorites-and-recent".into(),
                status: SettingEditStatus::Failed,
            })
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "[invalid");
    }

    #[test]
    fn saved_unknown_completion_retains_registrations_input_and_requires_retained_loader_recovery() {
        let scratch = Scratch::new();
        let directories = StandardDirectories::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::new()
                .set("HOME", &scratch.0)
                .set("APPDATA", &scratch.0)
                .set("LOCALAPPDATA", &scratch.0)
                .set("CRIKEY_CONFIG_DIR", &scratch.0),
        )
        .unwrap();
        let policy = scratch.0.join("custom-policy.toml");
        std::fs::write(&policy, "[launcher]\nmax-results = 17\n").unwrap();
        let overrides = vec![("launcher.show-hints".into(), "false".into())];
        let config = ConfigStore::load_with_overrides(&directories, Some(&policy), &overrides).unwrap();
        let published = Mutex::new(CommandPreferencesStore::load(&config).unwrap());
        let original = published.lock().unwrap().snapshot();
        let mut state = WorkerConfiguration {
            watch: Some(config.source_watch()),
            config: Some(config.clone()),
            authoritative: true,
            directories,
            session_overrides: Vec::new(),
            settings: SettingsAuthority::default(),
        };
        let mut desired = config.clone();
        desired
            .commit_user_global("launcher.command-home", "favorites", &config.revision())
            .unwrap();
        let saved = std::fs::read(config.config_path()).unwrap();
        // Exercise the typed post-persistence failure boundary with an actual
        // durable desired document and an unavailable retained policy source.
        std::fs::write(&policy, "[invalid").unwrap();
        let unknown = state.persisted_reload(config.revision());
        assert_eq!(unknown.state, CompletionState::PersistedReloadRequired);
        assert!(unknown.preferences.is_none());
        assert_eq!(
            edit_status(false, unknown.state),
            crikey_ui::SettingEditStatus::PersistedReloadRequired
        );
        assert!(!state.accepts(&desired));
        assert_eq!(published.lock().unwrap().snapshot().revision, original.revision);
        let unavailable = state.reload(&published);
        assert_eq!(unavailable.state, CompletionState::Unavailable);
        assert!(unavailable.preferences.is_none());
        assert_eq!(std::fs::read(config.config_path()).unwrap(), saved);
        std::fs::write(&policy, "[launcher]\nmax-results = 17\n").unwrap();
        let recovered = state.reload(&published);
        assert_eq!(recovered.state, CompletionState::Authoritative);
        let effective = recovered.config.unwrap();
        assert_eq!(effective.get("launcher.command-home"), Some("favorites"));
        assert_eq!(effective.get("launcher.max-results"), Some("17"));
        assert_eq!(effective.get("launcher.show-hints"), Some("false"));
        assert!(state.accepts(&effective));
        assert_eq!(std::fs::read(config.config_path()).unwrap(), saved);
    }

    #[test]
    fn activation_save_rejects_shadowed_durable_command_binding_without_writing() {
        use crikey_config::command_preferences::{CommandPreference, CommandPreferences};
        use std::time::Duration;
        let scratch = Scratch::new();
        let directories = StandardDirectories::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::new()
                .set("HOME", &scratch.0)
                .set("APPDATA", &scratch.0)
                .set("LOCALAPPDATA", &scratch.0)
                .set("CRIKEY_CONFIG_DIR", &scratch.0),
        )
        .unwrap();
        let durable = CommandPreferences {
            records: vec![CommandPreference {
                reference: crikey_core::command::builtin_commands().remove(0).reference,
                hidden: false,
                alias: None,
                hotkey: Some("Ctrl+Shift+G".into()),
            }],
            favorites: Vec::new(),
            fallbacks: Vec::new(),
        };
        let mut config = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
        config.set_user_global(KEY_COMMAND_PREFERENCES, &durable.encode().unwrap());
        config.save().unwrap();
        let path = config.config_path().to_owned();
        let disk_before = std::fs::read(&path).unwrap();
        config.set_session_override(
            KEY_COMMAND_PREFERENCES,
            &CommandPreferences::default().encode().unwrap(),
        );
        let store = Arc::new(Mutex::new(CommandPreferencesStore::empty()));
        let worker = PreferenceWorker::spawn(
            store,
            Some(config),
            directories.clone(),
            Vec::new(),
            None,
            Vec::new(),
            || {},
        )
        .unwrap();
        let startup = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(startup
            .preferences
            .as_ref()
            .unwrap()
            .preferences
            .records
            .is_empty());
        let before = startup.config.unwrap();
        let activation_before = launcher_settings::configured_hotkey(Some(&before));
        worker
            .submit_setting(
                41,
                before,
                crikey_config::KEY_ACTIVATION_HOTKEY.into(),
                "Shift+Ctrl+G".into(),
            )
            .unwrap();
        let refused = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(refused.state, CompletionState::Authoritative);
        assert_eq!(
            refused.setting_edit,
            Some(crikey_ui::SettingEditOutcome {
                request_id: 41,
                key: crikey_config::KEY_ACTIVATION_HOTKEY.into(),
                value: "Shift+Ctrl+G".into(),
                status: crikey_ui::SettingEditStatus::Failed,
            })
        );
        assert_eq!(
            launcher_settings::configured_hotkey(refused.config.as_ref()),
            activation_before
        );
        assert_eq!(std::fs::read(&path).unwrap(), disk_before);
        let restarted = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
        assert_eq!(
            restarted.command_preferences_at(ConfigLayer::UserGlobal).unwrap(),
            Some(durable)
        );
        assert_eq!(
            launcher_settings::configured_hotkey(Some(&restarted)),
            activation_before
        );
    }

    #[test]
    fn activation_save_resolves_retained_profile_winners_not_inactive_documents() {
        use crikey_config::command_preferences::{CommandPreference, CommandPreferences};
        use crikey_ui::SettingEditStatus;
        use std::time::Duration;
        for (profile_has_command, profile_activation, expected) in [
            (true, None, SettingEditStatus::Failed),
            (false, None, SettingEditStatus::Saved),
            (true, Some("Ctrl+Shift+H"), SettingEditStatus::Saved),
        ] {
            let scratch = Scratch::new();
            let directories = StandardDirectories::resolve(
                DirectoryConvention::current(),
                &DirectoryEnvironment::new()
                    .set("HOME", &scratch.0)
                    .set("APPDATA", &scratch.0)
                    .set("LOCALAPPDATA", &scratch.0)
                    .set("CRIKEY_CONFIG_DIR", &scratch.0),
            )
            .unwrap();
            let command = CommandPreferences {
                records: vec![CommandPreference {
                    reference: crikey_core::command::builtin_commands().remove(0).reference,
                    hidden: false,
                    alias: None,
                    hotkey: Some("Ctrl+Shift+G".into()),
                }],
                favorites: Vec::new(),
                fallbacks: Vec::new(),
            };
            let empty = CommandPreferences::default();
            let profile_commands = if profile_has_command { &command } else { &empty };
            let profile_dir = directories.config_dir().join("profiles");
            std::fs::create_dir_all(&profile_dir).unwrap();
            let activation = profile_activation
                .map(|chord| format!("[launcher]\nactivation-hotkey = '{chord}'\n"))
                .unwrap_or_default();
            std::fs::write(
                profile_dir.join("work.toml"),
                format!(
                    "{activation}[commands]\npreferences = '''\n{}'''\n",
                    profile_commands.encode().unwrap()
                ),
            )
            .unwrap();
            let mut config = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
            config.set_user_global("launcher.profile", "work");
            config.set_user_global(KEY_COMMAND_PREFERENCES, &command.encode().unwrap());
            config.save().unwrap();
            let path = config.config_path().to_owned();
            let disk_before = std::fs::read(&path).unwrap();
            config.set_session_override(KEY_COMMAND_PREFERENCES, &empty.encode().unwrap());
            let store = Arc::new(Mutex::new(CommandPreferencesStore::empty()));
            let worker = PreferenceWorker::spawn(
                store,
                Some(config),
                directories.clone(),
                Vec::new(),
                None,
                Vec::new(),
                || {},
            )
            .unwrap();
            let initial = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            worker
                .submit_setting(
                    51,
                    initial.config.unwrap(),
                    crikey_config::KEY_ACTIVATION_HOTKEY.into(),
                    "Ctrl+Shift+G".into(),
                )
                .unwrap();
            let completion = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(completion.setting_edit.unwrap().status, expected);
            if expected == SettingEditStatus::Failed {
                assert_eq!(std::fs::read(&path).unwrap(), disk_before);
            }
            let restarted = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
            assert_eq!(
                restarted.layer_of(KEY_COMMAND_PREFERENCES),
                Some(ConfigLayer::Profile)
            );
            assert_eq!(
                CommandPreferences::decode(restarted.get(KEY_COMMAND_PREFERENCES).unwrap()).unwrap(),
                *profile_commands
            );
            assert!(restarted.validate_command_hotkey_conflicts().is_ok());
            if let Some(activation) = profile_activation {
                assert_eq!(launcher_settings::configured_hotkey(Some(&restarted)), activation);
            }
        }
    }

    #[test]
    fn command_hotkey_save_checks_retained_activation_without_rejecting_shadowed_commands() {
        use crikey_config::command_preferences::{CommandPreference, CommandPreferences};
        use crikey_ui::{CommandPreferenceEdit, SettingEditStatus};
        use std::time::Duration;
        for profile_shadows_commands in [false, true] {
            let scratch = Scratch::new();
            let directories = StandardDirectories::resolve(
                DirectoryConvention::current(),
                &DirectoryEnvironment::new()
                    .set("HOME", &scratch.0)
                    .set("APPDATA", &scratch.0)
                    .set("LOCALAPPDATA", &scratch.0)
                    .set("CRIKEY_CONFIG_DIR", &scratch.0),
            )
            .unwrap();
            let reference = crikey_core::command::builtin_commands().remove(0).reference;
            let mut config = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
            let preferences = CommandPreferences {
                records: vec![CommandPreference {
                    reference: reference.clone(),
                    hidden: false,
                    alias: None,
                    hotkey: None,
                }],
                favorites: Vec::new(),
                fallbacks: Vec::new(),
            };
            config.set_user_global(KEY_COMMAND_PREFERENCES, &preferences.encode().unwrap());
            config.set_user_global(crikey_config::KEY_ACTIVATION_HOTKEY, "Ctrl+Shift+G");
            if profile_shadows_commands {
                let profile_dir = directories.config_dir().join("profiles");
                std::fs::create_dir_all(&profile_dir).unwrap();
                std::fs::write(
                    profile_dir.join("work.toml"),
                    format!(
                        "[commands]\npreferences = '''\n{}'''\n",
                        CommandPreferences::default().encode().unwrap()
                    ),
                )
                .unwrap();
                config.set_user_global("launcher.profile", "work");
            }
            config.save().unwrap();
            let path = config.config_path().to_owned();
            let disk_before = std::fs::read(&path).unwrap();
            config.set_session_override(crikey_config::KEY_ACTIVATION_HOTKEY, "Ctrl+Shift+H");
            let store = Arc::new(Mutex::new(CommandPreferencesStore::empty()));
            let worker = PreferenceWorker::spawn(
                store,
                Some(config),
                directories.clone(),
                Vec::new(),
                None,
                Vec::new(),
                || {},
            )
            .unwrap();
            let initial = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            worker
                .submit_mutation(MutationRequest {
                    request_id: 61,
                    config: initial.config.unwrap(),
                    registry: Arc::new(CommandRegistrySnapshot::default()),
                    expected_revision: initial.preferences.unwrap().revision,
                    reference,
                    edit: CommandPreferenceEdit::SetHotkey(Some("Ctrl+Shift+G".into())),
                    session: false,
                })
                .unwrap();
            let completion = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            let expected = if profile_shadows_commands {
                SettingEditStatus::Saved
            } else {
                SettingEditStatus::Failed
            };
            assert_eq!(completion.command_edit.unwrap().status, expected);
            let restarted = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
            assert!(restarted.validate_command_hotkey_conflicts().is_ok());
            if profile_shadows_commands {
                assert!(completion.preferences.unwrap().preferences.records.is_empty());
                assert_eq!(
                    restarted
                        .command_preferences_at(ConfigLayer::UserGlobal)
                        .unwrap()
                        .unwrap()
                        .records[0]
                        .hotkey
                        .as_deref(),
                    Some("Ctrl+Shift+G")
                );
            } else {
                assert_eq!(std::fs::read(&path).unwrap(), disk_before);
            }
        }
    }
}

#[cfg(test)]
mod layer_consumer_tests {
    use super::*;
    use crikey_config::command_preferences::{CommandPreference, CommandPreferences};
    use crikey_core::{CommandRef, PluginId};
    use crikey_platform::{DirectoryConvention, DirectoryEnvironment};
    use crikey_ui::{CommandPreferenceEdit, UiEffect};
    use std::time::Duration;

    #[test]
    fn semantic_worker_edits_only_userglobal_without_promoting_session_preferences() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let scratch =
            Scratch(std::env::temp_dir().join(format!("crikey-semantic-preferences-{}", std::process::id())));
        std::fs::create_dir_all(&scratch.0).unwrap();
        let directories = StandardDirectories::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::new()
                .set("HOME", &scratch.0)
                .set("APPDATA", &scratch.0)
                .set("LOCALAPPDATA", &scratch.0)
                .set("CRIKEY_CONFIG_DIR", &scratch.0),
        )
        .unwrap();
        let reference = |owner: &str| CommandRef {
            provider_id: PluginId(owner.into()),
            command_id: "shared".into(),
        };
        let selected = reference("saved.owner");
        let global_other = reference("global.owner");
        let session_only = reference("session.owner");
        let mut expected_global = CommandPreferences {
            records: vec![
                CommandPreference {
                    reference: selected.clone(),
                    hidden: false,
                    alias: Some("global-alias".into()),
                    hotkey: Some("Ctrl+Shift+G".into()),
                },
                CommandPreference {
                    reference: global_other.clone(),
                    hidden: true,
                    alias: Some("occupied".into()),
                    hotkey: Some("Ctrl+Shift+O".into()),
                },
            ],
            favorites: vec![global_other.clone(), selected.clone()],
            fallbacks: vec![selected.clone(), global_other],
        };
        let session = CommandPreferences {
            records: vec![CommandPreference {
                reference: selected.clone(),
                hidden: false,
                alias: Some("session-private-alias".into()),
                hotkey: Some("Ctrl+Shift+S".into()),
            }],
            favorites: vec![session_only.clone()],
            fallbacks: vec![session_only],
        };
        let mut config = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
        config.set_user_global(KEY_COMMAND_PREFERENCES, &expected_global.encode().unwrap());
        config.save().unwrap();
        config.set_session_override(KEY_COMMAND_PREFERENCES, &session.encode().unwrap());
        let store = Arc::new(Mutex::new(CommandPreferencesStore::empty()));
        let worker = PreferenceWorker::spawn(
            store,
            Some(config),
            directories.clone(),
            Vec::new(),
            None,
            Vec::new(),
            || {},
        )
        .unwrap();
        let mut current = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(current.report.is_none());
        let registry = Arc::new(CommandRegistrySnapshot::default());
        for edit in [
            CommandPreferenceEdit::Hidden(true),
            CommandPreferenceEdit::SetAlias(Some("new-alias".into())),
            CommandPreferenceEdit::SetHotkey(Some("Ctrl+Shift+N".into())),
            CommandPreferenceEdit::SetAlias(None),
            CommandPreferenceEdit::SetHotkey(None),
            CommandPreferenceEdit::Favorite(false),
            CommandPreferenceEdit::Fallback(false),
        ] {
            let effect = UiEffect::MutateCommandPreferences {
                reference: selected.clone(),
                request_id: 1,
                expected_revision: current.preferences.as_ref().unwrap().revision,
                edit: edit.clone(),
            };
            let UiEffect::MutateCommandPreferences {
                request_id,
                reference,
                expected_revision,
                edit: semantic_edit,
            } = effect
            else {
                unreachable!();
            };
            worker
                .submit_mutation(MutationRequest {
                    request_id,
                    config: current.config.take().unwrap(),
                    registry: Arc::clone(&registry),
                    expected_revision,
                    reference,
                    edit: semantic_edit,
                    session: false,
                })
                .unwrap();
            current = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            let preferences = current.preferences.as_ref().expect("semantic edit must commit");
            assert_eq!(current.state, CompletionState::Authoritative);
            let ack = current.command_edit.as_ref().unwrap();
            assert_eq!(ack.request_id, 1);
            assert_eq!(ack.edit, edit);
            assert_eq!(ack.status, crikey_ui::SettingEditStatus::Saved);
            assert_eq!(preferences.preferences, session);
            assert_eq!(preferences.layer, Some(ConfigLayer::SessionOverride));
            assert_eq!(
                current.report.as_deref(),
                Some(
                    format!(
                        "Command preferences saved; effective layer: {}",
                        ConfigLayer::SessionOverride.as_str()
                    )
                    .as_str()
                )
            );
            match edit {
                CommandPreferenceEdit::Hidden(hidden) => expected_global.records[0].hidden = hidden,
                CommandPreferenceEdit::SetAlias(alias) => expected_global.records[0].alias = alias,
                CommandPreferenceEdit::SetHotkey(hotkey) => expected_global.records[0].hotkey = hotkey,
                CommandPreferenceEdit::Favorite(false) => {
                    expected_global.favorites.retain(|saved| saved != &selected)
                }
                CommandPreferenceEdit::Fallback(false) => {
                    expected_global.fallbacks.retain(|saved| saved != &selected)
                }
                _ => unreachable!(),
            }
            let durable = crate::LauncherConfiguration::load_store(&directories, &[]).unwrap();
            assert_eq!(
                durable.command_preferences_at(ConfigLayer::UserGlobal).unwrap(),
                Some(expected_global.clone())
            );
            assert_eq!(
                durable
                    .command_preferences_at(ConfigLayer::SessionOverride)
                    .unwrap(),
                None
            );
        }
        let path = current.config.as_ref().unwrap().config_path().to_owned();
        let disk_before = std::fs::read(&path).unwrap();
        for (index, edit) in [
            CommandPreferenceEdit::SetAlias(Some("OCCUPIED".into())),
            CommandPreferenceEdit::SetHotkey(Some("Shift+Ctrl+O".into())),
            CommandPreferenceEdit::SetHotkey(Some(launcher_settings::configured_hotkey(
                current.config.as_ref(),
            ))),
        ]
        .into_iter()
        .enumerate()
        {
            worker
                .submit_mutation(MutationRequest {
                    request_id: 100 + index as u64,
                    config: current.config.as_ref().unwrap().clone(),
                    registry: Arc::clone(&registry),
                    expected_revision: current.preferences.as_ref().unwrap().revision,
                    reference: selected.clone(),
                    edit: edit.clone(),
                    session: false,
                })
                .unwrap();
            let refused = worker.receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            let ack = refused.command_edit.unwrap();
            assert_eq!(ack.request_id, 100 + index as u64);
            assert_eq!(ack.edit, edit);
            assert_eq!(ack.status, crikey_ui::SettingEditStatus::Failed);
            assert_eq!(std::fs::read(&path).unwrap(), disk_before);
        }
    }
}
