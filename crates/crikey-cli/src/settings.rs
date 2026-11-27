//! The launcher's own settings surface, the activation hotkey it binds, and the
//! residency rules that make a dismiss survivable (spec 21.2).
//!
//! # Why these three live together
//!
//! They are one defect. A launcher that exits on dismiss is only usable while
//! its window is on screen; a launcher that stays resident is only usable while
//! its activation hotkey works; and a hotkey the platform refuses is only
//! recoverable if the user can reach a settings surface and choose another
//! chord. Splitting them across modules would let one be changed without the
//! other two, and the first hand test on Windows found exactly that gap: the
//! process ran, took no hotkey, exited on the first Escape, and left the owner
//! with no window, no shortcut and nowhere to look.
//!
//! # What the host owns
//!
//! `crikey-ui` owns presentation; `crikey-app::launcher_settings` owns the
//! shared declarations, readers and validation. This adapter formats terminal
//! settings and bridges the resident configuration/package authorities. The
//! composition root retains asynchronous OS shortcut registration and the
//! existing runtime owners; package leases never create replacement supervisors.

use std::fmt::Write as _;
use std::process::ExitCode;

#[cfg(test)]
use crikey_app::launcher_settings::{
    configured_hotkey, configured_rounded_corners, configured_show_hints, validate,
};
use crikey_app::launcher_settings::{rows, validate_edit};
use crikey_config::{ConfigStore, KEY_ACTIVATION_HOTKEY};
#[cfg(test)]
use crikey_config::{KEY_MAX_RESULTS, KEY_ROUNDED_CORNERS, KEY_SHOW_HINTS};
use crikey_ui::{LauncherViewModel, UiEffect};

pub(crate) fn apply_navigation_preferences(
    store: Option<&ConfigStore>,
    model: &mut LauncherViewModel,
    history: &mut crikey_app::QueryHistoryStore,
) {
    model.set_resume_delay(crikey_app::launcher_settings::configured_resume_delay(store));
    let mode = crikey_app::launcher_settings::configured_query_history(store);
    if mode == crikey_app::QueryHistoryMode::Off {
        model.set_query_recall(Vec::new());
    }
    if let Err(error) = history.set_mode(mode) {
        let message = format!("Query history preference failed: {error}");
        eprintln!("crikey: {message}");
        model.set_navigation_notice(message);
    }
}

/// What the source column says for a key no layer supplies.
///
/// `launcher.max-results` and `launcher.profile` have no built-in default, so
/// an unconfigured launcher genuinely has no value for them and must say so
/// rather than show an empty cell that reads as an empty string.
const UNSET: &str = "unset";

/// Writes one accepted setting through the user-global layer and persists it.
///
/// The same layer and the same file `crikey config` reports, so a value changed
/// in the panel is a value the command line agrees about. The returned sentence
/// is what the user is shown: it names the layer that actually wins, because
/// the user-global layer is not the top of the stack — a selected profile or a
/// `crikey run --set` override outranks it, and a panel that reported its own
/// write as effective would be lying about which chord the launcher answers to.
pub(crate) fn persist(store: &mut ConfigStore, key: &str, value: &str) -> Result<String, String> {
    let receipts = validate_edit(store, key, value)?;
    persist_typed(store, key, value, &receipts).map_err(|error| match error {
        crikey_config::ConfigError::Conflict => "Configuration changed; reload and retry this edit".into(),
        crikey_config::ConfigError::PersistedReload => {
            "Setting was saved, but the effective configuration could not be reloaded".into()
        }
        _ => format!("`{key}` could not be written"),
    })
}

/// The worker validates first and retains typed persistence/reload outcomes.
pub(crate) fn persist_typed(
    store: &mut ConfigStore,
    key: &str,
    value: &str,
    validated_sources: &[crikey_config::ConfigRevision],
) -> Result<String, crikey_config::ConfigError> {
    let expected = store.revision();
    store.commit_user_global_checked(key, value, &expected, validated_sources)?;
    let effective = store.get(key).unwrap_or_default();
    if effective == value {
        Ok(format!("{key} = {value}"))
    } else {
        let layer = store.layer_of(key).map_or(UNSET, |layer| layer.as_str());
        Ok(format!(
            "{key} saved as {value}, but {layer} still supplies {effective}"
        ))
    }
}

/// Arms the launcher to come up with the settings panel open on the hotkey row,
/// and returns the diagnostic the caller must also record.
///
/// A resident launcher with no working hotkey is the trap the first Windows
/// test fell into: the process is alive, nothing raises it, and nothing on
/// screen says so. Opening the panel on the row that has to change is the only
/// report the user can act on — the stderr line is discarded for a
/// GUI-subsystem process, and the startup log is only found by someone who
/// already suspects the problem.
pub(crate) fn surface_hotkey_failure(
    view_model: &mut LauncherViewModel,
    accelerator: &str,
    reason: &str,
) -> String {
    view_model.open_settings(Some(KEY_ACTIVATION_HOTKEY));
    format!(
        "crikey: the activation hotkey {accelerator} could not be registered: {reason}; CriKey \
         stays running and has opened its settings so another chord can be chosen"
    )
}

// ---------------------------------------------------------------------------
// Residency
// ---------------------------------------------------------------------------

/// What one UI effect does to the window and to the process.
///
/// Only a deliberate quit ends the process. Before this existed the dismiss and
/// execute paths exited whenever the activation hotkey had failed to register,
/// which turned a shortcut conflict on the user's desktop into a launcher that
/// disappeared on the first Escape and could only be started again from a
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Residency {
    /// Hide the window; the process keeps waiting for an activation.
    Hide,
    /// Tear the launcher down: stop the workers, release the launcher lock and
    /// leave.
    Exit,
}

/// The residency consequence of `effect`, or `None` when it has none.
///
/// Deliberately not a function of whether the activation hotkey is live. That
/// input is what produced the defect: it made the launcher's lifetime depend on
/// a condition the user could not see, so one Escape keystroke either hid the
/// window or ended the session depending on what else happened to be running on
/// their desktop.
pub(crate) fn residency(effect: &UiEffect) -> Option<Residency> {
    match effect {
        UiEffect::Dismissed => Some(Residency::Hide),
        UiEffect::Quit => Some(Residency::Exit),
        // A page keeps the launcher exactly where it is. Closing one returns
        // the user to their results, which is a change of surface and not a
        // reason to hide the window or end the process.
        UiEffect::Query(_)
        | UiEffect::RecallQuery
        | UiEffect::ClearQueryHistory
        | UiEffect::InputBatch { .. }
        | UiEffect::Execute { .. }
        | UiEffect::ExecuteStandard { .. }
        | UiEffect::RefreshStandard(_)
        | UiEffect::OpenStandardLink { .. }
        | UiEffect::InvokeCommand { .. }
        | UiEffect::MutateCommandPreferences { .. }
        | UiEffect::ResetCommandRanking { .. }
        | UiEffect::OpenPreviewLink { .. }
        | UiEffect::SetSetting { .. }
        | UiEffect::ManageSettings(_)
        | UiEffect::PageInput(_)
        | UiEffect::WebInput(_)
        | UiEffect::WebIme(_)
        | UiEffect::ResizePage { .. }
        | UiEffect::ClosePage => None,
        UiEffect::ExecuteActionless { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// `crikey settings`
// ---------------------------------------------------------------------------

/// A completed operation that found nothing wrong.
const EX_OK: u8 = 0;
/// A completed operation that could not read the configuration.
const EX_INVALID: u8 = 1;
/// An argument list this module could not parse.
const EX_USAGE: u8 = 64;

pub(crate) const SETTINGS_USAGE: &str = "\
crikey settings - report and change the launcher's own settings

USAGE:
    crikey settings
    crikey settings set <KEY> <VALUE>
    crikey settings --help

Reports every setting the launcher's settings panel offers: its key, the label
the panel shows, the layer that supplied the winning value, and that value. A
setting no layer supplies is reported with layer=unset.

`set` writes through the same user-global layer the panel writes, so a value
changed here and a value changed in the panel are the same value. A running
launcher picks the change up on its next configuration reload. If a higher
layer - a selected profile, or a `crikey run --set` override - still supplies
a different value, that is said rather than hidden.
";

/// `crikey settings` — the panel's rows, on a terminal.
///
/// The same rows and the same effective values the panel shows, in the
/// whitespace-separated `key=value` shape every other CriKey command prints, so
/// the settings surface is reachable on a machine where the graphical one is
/// exactly what is broken.
pub(crate) fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        None => {}
        Some("-h" | "--help") if args.len() == 1 => {
            print!("{SETTINGS_USAGE}");
            return ExitCode::from(EX_OK);
        }
        Some("set") => return set(&args[1..]),
        Some(other) => {
            eprintln!("crikey: `settings` takes no arguments, got `{other}`\n\n{SETTINGS_USAGE}");
            return ExitCode::from(EX_USAGE);
        }
    }
    let store = match crate::config_commands::load() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };
    for row in rows(Some(&store)) {
        println!(
            "key={} label={} layer={} value={}",
            encode(&row.key),
            encode(&row.label),
            encode(&row.source),
            encode(&row.value)
        );
    }
    ExitCode::from(EX_OK)
}

/// `crikey settings set <KEY> <VALUE>` — the panel's edit, on a terminal.
///
/// It refuses an unknown key rather than writing it, because a launcher setting
/// nothing reads is indistinguishable from a typo, and a typo silently accepted
/// here is a hotkey the user believes they changed.
fn set(args: &[String]) -> ExitCode {
    let [key, value] = args else {
        eprintln!("crikey: `settings set` takes exactly a key and a value\n\n{SETTINGS_USAGE}");
        return ExitCode::from(EX_USAGE);
    };
    let mut store = match crate::config_commands::load() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };
    match persist(&mut store, key, value) {
        Ok(message) => {
            println!("{message}");
            ExitCode::from(EX_OK)
        }
        Err(message) => {
            eprintln!("crikey: {message}");
            ExitCode::from(EX_INVALID)
        }
    }
}

/// Mirrors `config_commands::encode` exactly (spec §28 output contract).
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

pub(crate) fn closed_runtime_effect(effect: &UiEffect, owners: &[crikey_core::PluginId]) -> bool {
    match effect {
        // The dispatch loop checks each inner effect separately.
        UiEffect::InputBatch { .. } => false,
        UiEffect::ExecuteStandard { owner, .. }
        | UiEffect::OpenStandardLink { owner, .. }
        | UiEffect::ExecuteActionless { owner, .. }
        | UiEffect::Execute { owner, .. } => owners.contains(owner),
        UiEffect::InvokeCommand { reference, .. } => owners.contains(&reference.provider_id),
        UiEffect::OpenPreviewLink { target, .. } => owners.contains(&target.plugin),
        UiEffect::RefreshStandard(_)
        | UiEffect::PageInput(_)
        | UiEffect::WebInput(_)
        | UiEffect::WebIme(_)
        | UiEffect::ResizePage { .. } => true,
        UiEffect::Query(_)
        | UiEffect::RecallQuery
        | UiEffect::ClearQueryHistory
        | UiEffect::Dismissed
        | UiEffect::ManageSettings(_)
        | UiEffect::SetSetting { .. }
        | UiEffect::MutateCommandPreferences { .. }
        | UiEffect::ResetCommandRanking { .. }
        | UiEffect::ClosePage
        | UiEffect::Quit => false,
    }
}

/// Process-scoped consumer state; closing a logical view never owns/cancels a job.
pub(crate) struct Management {
    pub panel: crikey_ui::settings_management::Panel,
    ids: std::collections::BTreeMap<u64, crikey_app::package_jobs::JobId>,
    jobs: std::collections::BTreeMap<u64, crikey_ui::settings_management::Job>,
    edit: Option<(String, u64)>,
    query: String,
    plugin: Option<crikey_core::PluginId>,
    refresh_pending: bool,
    focus: Option<(crikey_ui::ViewId, Option<String>)>,
    pending_category: Option<crikey_ui::settings_management::Category>,
}
impl Management {
    pub fn new() -> Self {
        Self {
            panel: Default::default(),
            ids: Default::default(),
            jobs: Default::default(),
            edit: None,
            query: String::new(),
            plugin: None,
            refresh_pending: false,
            focus: None,
            pending_category: None,
        }
    }
    pub fn search(
        &mut self,
        view: crikey_ui::ViewId,
        operation: super::command_preferences::SettingsOperation,
        worker: &super::command_preferences::PreferenceWorker,
        expected: Option<(u64, u64)>,
    ) {
        match &operation {
            super::command_preferences::SettingsOperation::Search { text } => {
                self.query.clone_from(text);
                self.plugin = None;
                self.edit = None;
            }
            super::command_preferences::SettingsOperation::BrowsePlugin { plugin } => {
                self.plugin = Some(plugin.clone());
                self.edit = None;
            }
            super::command_preferences::SettingsOperation::Edit { .. } => (),
        }
        self.panel.view = Some(view);
        self.panel.request = crikey_ui::next_setting_edit_id();
        let (config, schema) = expected.unwrap_or((self.panel.config, 0));
        self.panel.loading = true;
        if worker
            .submit_settings(super::command_preferences::SettingsRequest {
                view,
                request_id: self.panel.request,
                expected_config: config,
                expected_schema: schema,
                operation,
            })
            .is_err()
        {
            self.panel.loading = false;
            self.panel.notice = "Configuration authority is busy or unavailable; no edit was accepted".into();
        }
    }
    pub fn refresh(
        &mut self,
        view: crikey_ui::ViewId,
        worker: &super::command_preferences::PreferenceWorker,
    ) {
        if self.panel.loading {
            self.refresh_pending = true;
            return;
        }
        let operation = self.plugin.clone().map_or_else(
            || super::command_preferences::SettingsOperation::Search {
                text: self.query.clone(),
            },
            |plugin| super::command_preferences::SettingsOperation::BrowsePlugin { plugin },
        );
        self.search(view, operation, worker, None);
    }
    fn select_category(
        &mut self,
        view: crikey_ui::ViewId,
        category: crikey_ui::settings_management::Category,
        worker: &super::command_preferences::PreferenceWorker,
    ) {
        self.panel.category = category;
        self.pending_category = None;
        self.search(
            view,
            super::command_preferences::SettingsOperation::Search { text: String::new() },
            worker,
            None,
        );
        self.panel.query_reset = self.panel.request;
    }
    pub fn route_focus(
        &mut self,
        view: crikey_ui::ViewId,
        focus: Option<&str>,
        worker: &super::command_preferences::PreferenceWorker,
    ) -> bool {
        use crikey_ui::settings_management::Category;
        if self
            .focus
            .as_ref()
            .is_some_and(|(old_view, old_focus)| *old_view == view && old_focus.as_deref() == focus)
        {
            return false;
        }
        self.focus = Some((view, focus.map(str::to_owned)));
        let category = focus.and_then(|focus| {
            let focus = focus.strip_prefix("record:").unwrap_or(focus);
            crikey_app::launcher_settings::declaration(focus)
                .and_then(|declaration| Category::from_metadata(declaration.category))
                .or_else(|| focus.starts_with("command:").then_some(Category::Shortcuts))
        });
        let Some(category) = category else {
            return false;
        };
        self.pending_category = Some(category);
        // Keep the existing bootstrap recovery rows visible until real metadata
        // arrives; no UI-owned key/category table authorizes fallback fields.
        if self.panel.schema != 0 || category == Category::Shortcuts {
            self.panel.category = category;
        }
        self.search(
            view,
            super::command_preferences::SettingsOperation::Search { text: String::new() },
            worker,
            None,
        );
        self.panel.query_reset = self.panel.request;
        true
    }
    fn current_package_target(&self, request: &crikey_ui::settings_management::Request) -> bool {
        use crikey_ui::settings_management::Request;
        match request {
            Request::Remove { kind, id } => self
                .panel
                .packages
                .iter()
                .any(|row| row.installed && row.kind == Some(*kind) && &row.id == id),
            Request::Enable { plugin, .. } => self
                .panel
                .packages
                .iter()
                .any(|row| row.enabled.is_some() && row.plugin.as_ref() == Some(plugin)),
            Request::Install(id) => self
                .panel
                .packages
                .iter()
                .any(|row| !row.installed && row.plugin.is_none() && &row.id == id),
            Request::Details(target) => self.panel.packages.iter().any(|row| match target {
                crikey_ui::settings_management::PackageDetailsTarget::Index { id } => {
                    row.plugin.is_none() && &row.id == id
                }
                crikey_ui::settings_management::PackageDetailsTarget::Local { kind, id } => {
                    row.plugin.is_some() && row.kind == Some(*kind) && &row.id == id
                }
            }),
            Request::Search(_)
            | Request::Category(_)
            | Request::BrowsePlugin(_)
            | Request::LegacyConfiguration(_)
            | Request::Edit { .. }
            | Request::Packages
            | Request::PackageSearch(_)
            | Request::UpdateIndex
            | Request::InstallFrom { .. }
            | Request::Diagnose(_)
            | Request::Cancel(_) => true,
        }
    }
    pub fn request(
        &mut self,
        request: crikey_ui::settings_management::Request,
        view: crikey_ui::ViewId,
        worker: &super::command_preferences::PreferenceWorker,
        packages: &crikey_app::package_jobs::PackageJobs,
    ) {
        use super::command_preferences::{SettingsOperation as S, SettingsTarget};
        use crikey_app::package_jobs::{PackageOperation as P, PackageShowTarget};
        use crikey_ui::settings_management::{PackageDetailsTarget, Request as R};
        if !self.current_package_target(&request) {
            self.panel.notice = "Package inventory changed; refresh and select the operation again".into();
            return;
        }
        let operation = match request {
            R::Category(category) => {
                self.select_category(view, category, worker);
                return;
            }
            R::Search(text) => {
                self.pending_category = None;
                self.search(view, S::Search { text }, worker, None);
                return;
            }
            R::BrowsePlugin(plugin) => {
                self.pending_category = None;
                self.panel.category = crikey_ui::settings_management::Category::Plugins;
                self.search(view, S::BrowsePlugin { plugin }, worker, None);
                return;
            }
            R::LegacyConfiguration(id) => {
                self.panel.edit_notice = format!(
                    "Legacy configuration for {id}: settings are read from {id}.ini, or the declared main module's .ini, inside the active package directory. Legacy code can also manage files in the launcher user-configuration directory. These files retain their legacy syntax; schema-based plugin TOML edits do not rewrite them. Follow this package's configuration instructions."
                ).into();
                return;
            }
            R::Edit {
                request_id,
                key,
                plugin,
                draft,
                config,
                schema,
            } => {
                self.edit = Some((key.clone(), request_id));
                self.search(
                    view,
                    S::Edit {
                        key,
                        target: plugin.map_or(SettingsTarget::UserGlobal, |plugin| {
                            SettingsTarget::PluginUser { plugin }
                        }),
                        draft,
                    },
                    worker,
                    Some((config, schema)),
                );
                return;
            }
            R::Packages => P::List,
            R::PackageSearch(text) => P::Search(text),
            R::Details(target) => P::Show(match target {
                PackageDetailsTarget::Index { id } => PackageShowTarget::Index { id },
                PackageDetailsTarget::Local { kind, id } => PackageShowTarget::Local { kind, id },
            }),
            R::UpdateIndex => P::UpdateIndex,
            R::Install(id) => P::InstallIndexed(id),
            R::InstallFrom { source, kind } => {
                use crikey_package_manager::InstallSource;
                use crikey_ui::settings_management::SourceKind;
                match kind {
                    SourceKind::Index => P::InstallIndexed(source),
                    SourceKind::Directory => P::Install(InstallSource::Directory(source.into())),
                    SourceKind::Archive => P::Install(InstallSource::Archive(source.into())),
                    SourceKind::Legacy => P::Install(InstallSource::LegacyPackage(source.into())),
                    SourceKind::Url => P::Install(InstallSource::Url(source)),
                }
            }
            R::Remove { kind, id } => P::Remove { kind, id },
            R::Enable { plugin, enabled } => {
                if enabled {
                    P::Enable(plugin.0)
                } else {
                    P::Disable(plugin.0)
                }
            }
            R::Diagnose(id) => P::Doctor(id),
            R::Cancel(id) => {
                use crikey_app::package_jobs::CancellationStatus;
                self.panel.notice = match self.ids.get(&id).map(|id| packages.cancel(*id)) {
                    Some(CancellationStatus::Requested) => {
                        "Cancellation requested; waiting for the actual terminal result"
                    }
                    Some(CancellationStatus::NotCancellable) => {
                        "Not cancellable during commit/reconciliation; the job is still running"
                    }
                    Some(CancellationStatus::AlreadyTerminal) => "Job already finished",
                    _ => "Job is no longer retained",
                }
                .into();
                return;
            }
        };
        // Explicit bounded history policy: evict only already delivered terminals.
        if self.ids.len() >= 128 {
            if let Some(id) = self.jobs.iter().find(|(_, job)| job.terminal).map(|(id, _)| *id) {
                if self.ids.get(&id).is_some_and(|job| packages.forget(*job)) {
                    self.ids.remove(&id);
                    self.jobs.remove(&id);
                }
            }
        }
        match packages.submit(operation) {
            Ok(id) => {
                self.ids.insert(crikey_ui::next_setting_edit_id(), id);
                self.panel.notice = "Background job accepted; closing Settings does not cancel it".into();
            }
            Err(_) => {
                self.panel.notice = "Package job refused: input, capacity or ownership unavailable".into()
            }
        }
    }
    pub fn drain(
        &mut self,
        current: Option<(crikey_ui::ViewId, crikey_ui::ViewKind)>,
        worker: &super::command_preferences::PreferenceWorker,
        packages: Option<&crikey_app::package_jobs::PackageJobs>,
    ) {
        use super::command_preferences::{SettingsFailure, SettingsPayload};
        while let Some(response) = worker.take_settings() {
            if current != Some((response.view, crikey_ui::ViewKind::Settings))
                || self.panel.view != Some(response.view)
                || self.panel.request != response.request_id
            {
                continue;
            }
            self.panel.loading = false;
            self.panel.config = response.config_revision;
            self.panel.schema = response.schema_revision;
            match response.result {
                Ok(SettingsPayload::Search { results, mut rows }) => {
                    self.panel.projection_config = response.config_revision;
                    self.panel.projection_schema = response.schema_revision;
                    self.panel.authoritative = true;
                    self.panel.schema_rejected = false;
                    if let Some(category) = self.pending_category.take() {
                        self.panel.category = category;
                    }
                    self.panel.fields = results
                        .hits
                        .into_iter()
                        .filter_map(|hit| {
                            let index = rows.iter().position(|row| row.key == hit.key)?;
                            let row = rows.swap_remove(index);
                            let declaration = crikey_app::launcher_settings::declaration(&hit.key);
                            let max_bytes = declaration.map_or(4096, |setting| setting.max_edit_bytes());
                            let control = if row.secret || !row.value_available {
                                crikey_ui::SettingControl::Text
                            } else {
                                match declaration.map(|setting| setting.control) {
                                    Some(crikey_app::launcher_settings::Control::Choice { options }) => {
                                        crikey_ui::SettingControl::Choice { options }
                                    }
                                    // An optional plugin boolean without a value stays
                                    // unset and uses the text editor until saved.
                                    _ if matches!(hit.control.as_str(), "boolean" | "bool")
                                        && (hit.key.starts_with("launcher.")
                                            || matches!(row.value.as_str(), "true" | "false")) =>
                                    {
                                        crikey_ui::SettingControl::Toggle {
                                            on: row.value != "false",
                                        }
                                    }
                                    _ => crikey_ui::SettingControl::Text,
                                }
                            };
                            Some(crikey_ui::settings_management::Field {
                                plugin: (!hit.key.starts_with("launcher."))
                                    .then_some(crikey_core::PluginId(hit.navigation_target)),
                                key: hit.key,
                                label: hit.label,
                                description: hit.description,
                                category: hit.category,
                                value: row.value,
                                source: row.source,
                                restart: row.requires_restart,
                                secret: row.secret,
                                control,
                                max_bytes,
                                value_available: row.value_available,
                            })
                        })
                        .collect::<Vec<_>>()
                        .into();
                    self.panel.notice = if results.truncated {
                        "Results truncated; narrow the search"
                    } else {
                        ""
                    }
                    .into();
                }
                Ok(SettingsPayload::Saved { row }) => {
                    let shadowed = row.source
                        != if row.key.starts_with("launcher.") {
                            "user-global"
                        } else {
                            "user-plugin"
                        };
                    self.panel.edit_notice = format!(
                        "Saved. {}Effective source: {}. {}",
                        if shadowed {
                            "The user setting is shadowed. "
                        } else {
                            ""
                        },
                        row.source,
                        if row.requires_restart {
                            "Next launch required."
                        } else {
                            "Live delivery is asynchronous; active shortcut binding is shown separately."
                        },
                    )
                    .into();
                    self.panel.acknowledged = self.edit.take();
                    let mut fields = self.panel.fields.to_vec();
                    if let Some(field) = fields.iter_mut().find(|field| field.key == row.key) {
                        field.value_available = row.value_available;
                        if matches!(field.control, crikey_ui::SettingControl::Toggle { .. }) {
                            field.control = if row.value_available {
                                crikey_ui::SettingControl::Toggle {
                                    on: row.value != "false",
                                }
                            } else {
                                crikey_ui::SettingControl::Text
                            };
                        }
                        field.value = row.value;
                        field.source = row.source;
                    }
                    self.panel.fields = fields.into();
                    self.refresh_pending = true;
                }
                Err(failure) => {
                    if failure == SettingsFailure::StaleSchema {
                        // The typed refusal itself revokes private editor state,
                        // even when a failed reload retains the last schema revision.
                        self.panel.schema_refusal = response.request_id;
                        self.panel.schema_rejected = true;
                    }
                    if matches!(
                        failure,
                        SettingsFailure::Unavailable
                            | SettingsFailure::PersistedReload
                            | SettingsFailure::StaleSchema
                    ) {
                        self.panel.authoritative = false;
                    }
                    self.panel.edit_notice = match failure {
                        SettingsFailure::Conflict => "Configuration changed. Draft retained; refresh settings and explicitly rebase before saving",
                        SettingsFailure::StaleSchema => "Setting declaration changed. Secret drafts were cleared; refresh before editing. Nonsecret drafts are retained",
                        SettingsFailure::Unavailable => "Configuration authority unavailable; no edit accepted",
                        SettingsFailure::Invalid => "Edit rejected by the setting declaration; draft retained",
                        SettingsFailure::PersistedReload => "Setting was saved, but effective reload failed. Do not assume the old value remains on disk",
                    }.into();
                }
            }
        }
        if self.refresh_pending && !self.panel.loading {
            self.refresh_pending = false;
            if let Some((view, crikey_ui::ViewKind::Settings)) = current {
                self.refresh(view, worker);
            }
        }
        let Some(packages) = packages else {
            return;
        };
        let mut jobs_changed = false;
        let mut refresh_inventory = false;
        for snapshot in packages.drain_events(128) {
            let Some(id) = self
                .ids
                .iter()
                .find(|(_, backend)| **backend == snapshot.id)
                .map(|(id, _)| *id)
            else {
                continue;
            };
            if self
                .jobs
                .get(&id)
                .is_some_and(|old| old.sequence >= snapshot.sequence)
            {
                continue;
            }
            jobs_changed = true;
            if snapshot.outcome.is_some()
                && matches!(snapshot.operation, "install" | "remove" | "enable" | "disable")
            {
                refresh_inventory = true;
            }
            if let Some(outcome) = snapshot.outcome.as_ref() {
                self.observe_package_outcome(outcome);
            }
            let status = self.package_job_status(
                snapshot.operation,
                snapshot.outcome.as_ref(),
                snapshot.phase,
                snapshot.measured_progress,
            );
            self.jobs.insert(
                id,
                crikey_ui::settings_management::Job {
                    id,
                    sequence: snapshot.sequence,
                    operation: snapshot.operation,
                    terminal: snapshot.outcome.is_some(),
                    status,
                },
            );
        }
        if refresh_inventory && self.ids.len() < 128 {
            if let Ok(id) = packages.submit(crikey_app::package_jobs::PackageOperation::List) {
                self.ids.insert(crikey_ui::next_setting_edit_id(), id);
            }
        }
        if jobs_changed {
            self.panel.jobs = self.jobs.values().cloned().collect::<Vec<_>>().into();
        }
    }

    fn package_job_status(
        &mut self,
        operation: &str,
        outcome: Option<&crikey_app::package_jobs::JobOutcome>,
        phase: crikey_app::package_jobs::JobPhase,
        progress: Option<(u64, u64)>,
    ) -> Arc<str> {
        use crikey_app::package_jobs::{JobOutcome, JobPhase};
        match outcome {
            Some(JobOutcome::Completed {
                report,
                restart_required,
                details_truncated,
                ..
            }) => {
                if *details_truncated {
                    self.panel.notice = "Job details truncated by the reporting limit".into();
                }
                if *restart_required {
                    self.panel.notice = if matches!(operation, "install" | "remove") {
                        "Operation finished. Third-party runtimes remain closed until next launch"
                    } else {
                        "Configuration saved. The runtime change requires the next launch"
                    }
                    .into();
                }
                report.clone()
            }
            Some(JobOutcome::Failed {
                error,
                reconciliation,
                restart_required,
            }) => {
                // Restart policy is not evidence of a successful configuration write.
                self.panel.notice = if reconciliation.is_some() {
                    "Reconciliation failed; plugin admission remains failed-closed"
                } else if *restart_required && matches!(operation, "install" | "remove") {
                    "Package operation failed; third-party runtimes remain closed until next launch"
                } else if error.code == "configuration-persisted-reload" {
                    "Configuration was persisted but effective reload failed; next launch required"
                } else {
                    "Operation failed; see job details"
                }
                .into();
                match reconciliation {
                    Some(reconciliation) => {
                        format!("{}\nReconciliation: {}", error.detail, reconciliation.detail).into()
                    }
                    None => error.detail.clone(),
                }
            }
            Some(JobOutcome::Cancelled) => "Cancelled".into(),
            None => match progress {
                Some((done, total)) => format!("{phase:?}: {done}/{total}").into(),
                None => match phase {
                    JobPhase::Queued => "Queued",
                    JobPhase::Working => "Working (indeterminate)",
                    JobPhase::Commit => "Committing — not cancellable",
                    JobPhase::Reconciling => "Reconciling actual disk/configuration state",
                    JobPhase::Terminal => "Terminal",
                }
                .into(),
            },
        }
    }
    fn observe_package_outcome(&mut self, outcome: &crikey_app::package_jobs::JobOutcome) {
        if let crikey_app::package_jobs::JobOutcome::Completed { rows: Some(rows), .. } = outcome {
            self.panel.packages = rows
                .iter()
                .map(|row| crikey_ui::settings_management::Package {
                    id: row.package_id.clone(),
                    plugin: row.plugin.clone(),
                    kind: row.kind,
                    name: row.name.clone(),
                    version: row.version.clone(),
                    summary: row.summary.clone(),
                    installed: row.installed,
                    enabled: row.enabled,
                })
                .collect::<Vec<_>>()
                .into();
            self.panel.packages_revision = self
                .panel
                .packages_revision
                .checked_add(1)
                .expect("package presentation revision exhausted");
        }
    }
}

use crikey_app::package_jobs::{JobError, PackageOwner, QuiescentPlugins};
use crikey_app::runtime_control::{
    AdmissionGate, DriverControl, PluginAdmission, StopTicket, StoppedRuntime,
};
use std::num::NonZeroU64;
use std::sync::{mpsc, Arc, Mutex};

pub(crate) enum WebSettlement {
    #[cfg(target_os = "linux")]
    Owned(super::web_host::WebStopTicket),
    /// Minted only by the composition owner inspecting its actual absent owner.
    NoOwner,
    Failed,
}
#[derive(Clone)]
struct ExpectedRuntime {
    runtime: &'static str,
    owners: Arc<std::collections::BTreeSet<crikey_core::PluginId>>,
    declared: usize,
}
impl ExpectedRuntime {
    fn capture(control: &DriverControl) -> Self {
        Self {
            runtime: control.runtime(),
            owners: Arc::new(control.startup_owners().iter().cloned().collect()),
            declared: control.startup_owners().len(),
        }
    }
    fn matches(&self, proof: &StoppedRuntime, epoch: NonZeroU64) -> bool {
        if proof.runtime() != self.runtime
            || proof.epoch() != epoch
            || self.owners.len() != self.declared
            || proof.owners().len() != self.declared
        {
            return false;
        }
        let stopped: std::collections::BTreeSet<_> = proof.owners().iter().collect();
        stopped.len() == self.declared && self.owners.iter().eq(stopped)
    }
}

pub(crate) struct StopBundle {
    pub epoch: NonZeroU64,
    tickets: Vec<(ExpectedRuntime, StopTicket)>,
    pub web: WebSettlement,
    pub failed: bool,
}
impl StopBundle {
    fn request(
        epoch: NonZeroU64,
        controls: &[DriverControl],
        expected: &[ExpectedRuntime],
        web: Result<WebSettlement, JobError>,
    ) -> Self {
        let mut failed = controls.len() != expected.len();
        let mut tickets = Vec::with_capacity(controls.len());
        for (census, control) in expected.iter().zip(controls) {
            match control.request_stop(epoch) {
                Ok(ticket) => tickets.push((census.clone(), ticket)),
                Err(_) => failed = true,
            }
        }
        let web = web.unwrap_or_else(|_| {
            failed = true;
            WebSettlement::Failed
        });
        Self {
            epoch,
            tickets,
            web,
            failed,
        }
    }
    pub fn settle(self) -> Result<Vec<StoppedRuntime>, JobError> {
        let mut failed = self.failed;
        let mut ledger = Vec::with_capacity(self.tickets.len());
        for (expected, ticket) in self.tickets {
            match ticket.wait() {
                Ok(proof) if expected.matches(&proof, self.epoch) => ledger.push(proof),
                _ => failed = true,
            }
        }
        match self.web {
            #[cfg(target_os = "linux")]
            WebSettlement::Owned(ticket) => match ticket.wait() {
                Ok(proof) if proof.epoch() == self.epoch => (),
                _ => failed = true,
            },
            WebSettlement::NoOwner => (),
            WebSettlement::Failed => failed = true,
        }
        if failed {
            Err(JobError::new(
                "runtime-stop-failed",
                "Runtime physical teardown failed; admission remains closed",
            ))
        } else {
            Ok(ledger)
        }
    }
}
pub(crate) struct BeginQuiescence {
    pub reply: mpsc::SyncSender<Result<StopBundle, JobError>>,
}
pub(crate) struct ResidentOwner {
    pub gate: AdmissionGate,
    sender: mpsc::SyncSender<BeginQuiescence>,
    wake: Arc<dyn Fn() + Send + Sync>,
    controls: Vec<DriverControl>,
    ledger: Arc<Mutex<Option<Vec<StoppedRuntime>>>>,
    expected: Arc<[ExpectedRuntime]>,
}
impl ResidentOwner {
    pub fn new(
        controls: Vec<DriverControl>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> (Arc<Self>, mpsc::Receiver<BeginQuiescence>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let expected = controls
            .iter()
            .map(ExpectedRuntime::capture)
            .collect::<Vec<_>>()
            .into();
        (
            Arc::new(Self {
                gate: Default::default(),
                sender,
                wake,
                controls,
                expected,
                ledger: Arc::default(),
            }),
            receiver,
        )
    }
    pub fn request_stop(&self, epoch: NonZeroU64, web: Result<WebSettlement, JobError>) -> StopBundle {
        StopBundle::request(epoch, &self.controls, &self.expected, web)
    }
}
impl PackageOwner for ResidentOwner {
    fn quiesce(&self) -> Result<Box<dyn QuiescentPlugins>, JobError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.sender
            .try_send(BeginQuiescence { reply })
            .map_err(|_| JobError::new("owner-unavailable", "Runtime owner is unavailable or saturated"))?;
        (self.wake)();
        let bundle = response
            .recv_timeout(std::time::Duration::from_secs(30))
            .map_err(|_| {
                JobError::new(
                    "owner-unavailable",
                    "Runtime owner did not transfer shutdown tickets before the deadline",
                )
            })??;
        let epoch = bundle.epoch;
        let result = (|| {
            // Settle every transferred owner even if an earlier one failed.
            let ledger = bundle.settle()?;
            if ledger.len() != self.expected.len() || !self.expected.iter().zip(&ledger).all(|(expected, proof)| expected.matches(proof, epoch))
                || self.controls.iter().any(|control| !matches!(control.admission().state(), PluginAdmission::Offline { epoch: stopped } if stopped == epoch)) {
                return Err(JobError::new("runtime-stop-failed", "Runtime teardown failed; package mutation refused and admission remains closed"));
            }
            Ok(ledger)
        })();
        match result {
            Ok(ledger) => {
                *self.ledger.lock().unwrap_or_else(|error| error.into_inner()) = Some(ledger);
                Ok(Box::new(ResidentLease {
                    epoch,
                    gate: self.gate.clone(),
                    controls: self.controls.clone(),
                    expected: Arc::clone(&self.expected),
                    ledger: Arc::clone(&self.ledger),
                    reconciled: false,
                }))
            }
            Err(error) => {
                self.gate.finish(epoch, false);
                Err(error)
            }
        }
    }
}
struct ResidentLease {
    epoch: NonZeroU64,
    gate: AdmissionGate,
    controls: Vec<DriverControl>,
    ledger: Arc<Mutex<Option<Vec<StoppedRuntime>>>>,
    reconciled: bool,
    expected: Arc<[ExpectedRuntime]>,
}
impl ResidentLease {
    fn valid(&self) -> bool {
        matches!(self.gate.state(), PluginAdmission::Closing { epoch } | PluginAdmission::Offline { epoch } if epoch == self.epoch)
            && self.controls.iter().all(|control| matches!(control.admission().state(), PluginAdmission::Offline { epoch } if epoch == self.epoch))
            && self.ledger.lock().unwrap_or_else(|error| error.into_inner()).as_ref().is_some_and(|ledger| {
                ledger.len() == self.expected.len() && self.expected.iter().zip(ledger).all(|(expected, proof)| expected.matches(proof, self.epoch))
            })
    }
}
impl QuiescentPlugins for ResidentLease {
    fn acknowledge_stopped(&mut self, _id: &str) -> Result<(), crikey_package_manager::PackageError> {
        if !self.valid() {
            return Err(crikey_package_manager::PackageError::Install(
                "Runtime stopped ledger unavailable".into(),
            ));
        }
        // The complete all-runtime proof covers every exact bare-ID namespace,
        // including a genuinely new ID with no previous owner. Never prefix-match.
        Ok(())
    }
    fn reconcile(
        &mut self,
        installed: Result<&[crikey_package_manager::InstalledPlugin], &crikey_package_manager::PackageError>,
        configuration: Result<&ConfigStore, &JobError>,
    ) -> Result<(), JobError> {
        let success = self.valid() && installed.is_ok() && configuration.is_ok();
        self.gate.finish(self.epoch, success);
        self.reconciled = success;
        if success {
            Ok(())
        } else {
            Err(JobError::new("reconciliation-failed", "Disk inventory or configuration reconciliation unavailable; plugin admission remains failed-closed"))
        }
    }
}
impl Drop for ResidentLease {
    fn drop(&mut self) {
        if !self.reconciled {
            self.gate.finish(self.epoch, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_config::ConfigLayer;
    use crikey_platform::{DirectoryConvention, DirectoryEnvironment, StandardDirectories};
    use crikey_ui::UiCommand;

    #[test]
    fn package_failure_notice_requires_evidence_of_a_persisted_write() {
        use crikey_app::package_jobs::{JobOutcome, JobPhase};
        let mut management = Management::new();
        for operation in ["enable", "disable"] {
            for code in ["not-found", "configuration-load", "configuration-save"] {
                // A previous successful write must not leave a saved acknowledgement
                // visible for the new, pre-write failure.
                management.panel.notice = "Configuration saved".into();
                let outcome = JobOutcome::Failed {
                    error: JobError::new(code, "original failure details"),
                    reconciliation: None,
                    restart_required: true,
                };
                let status =
                    management.package_job_status(operation, Some(&outcome), JobPhase::Terminal, None);
                assert_eq!(&*status, "original failure details");
                assert_eq!(&*management.panel.notice, "Operation failed; see job details");
            }
            for restart_required in [false, true] {
                let outcome = JobOutcome::Failed {
                    error: JobError::new("configuration-persisted-reload", "reload failure details"),
                    reconciliation: None,
                    restart_required,
                };
                let status =
                    management.package_job_status(operation, Some(&outcome), JobPhase::Terminal, None);
                assert_eq!(&*status, "reload failure details");
                assert_eq!(
                    &*management.panel.notice,
                    "Configuration was persisted but effective reload failed; next launch required",
                );
            }
        }
    }

    #[test]
    fn package_failure_notice_preserves_runtime_closure_and_reconciliation_priority() {
        use crikey_app::package_jobs::{JobOutcome, JobPhase};
        let mut management = Management::new();
        for operation in ["install", "remove"] {
            let outcome = JobOutcome::Failed {
                error: JobError::new("install", "original package error"),
                reconciliation: None,
                restart_required: true,
            };
            let status = management.package_job_status(operation, Some(&outcome), JobPhase::Terminal, None);
            assert_eq!(&*status, "original package error");
            assert_eq!(
                &*management.panel.notice,
                "Package operation failed; third-party runtimes remain closed until next launch",
            );
        }
        for operation in ["install", "remove", "enable", "disable"] {
            let outcome = JobOutcome::Failed {
                error: JobError::new("configuration-persisted-reload", "original operation error"),
                reconciliation: Some(JobError::new("reconcile", "reconciliation details")),
                restart_required: true,
            };
            let status = management.package_job_status(operation, Some(&outcome), JobPhase::Terminal, None);
            assert!(status.contains("original operation error"));
            assert!(status.contains("reconciliation details"));
            assert_eq!(
                &*management.panel.notice,
                "Reconciliation failed; plugin admission remains failed-closed",
            );
        }
    }
    #[test]
    fn operator_discovered_packages_remain_configurable_but_are_not_managed_targets() {
        use crikey_app::package_jobs::{JobOutcome, PackageRow};
        use crikey_platform::PluginKind;
        use crikey_ui::settings_management::Request;
        let plugin = crikey_core::PluginId("native.operator".into());
        let mut management = Management::new();
        management.observe_package_outcome(&JobOutcome::Completed {
            report: Arc::from(""),
            restart_required: false,
            details_truncated: false,
            rows: Some(Arc::from([PackageRow {
                package_id: "operator".into(),
                plugin: Some(plugin.clone()),
                kind: Some(PluginKind::Native),
                name: String::new(),
                version: "1".into(),
                summary: String::new(),
                installed: false,
                enabled: Some(true),
            }])),
        });
        assert!(management.current_package_target(&Request::Enable {
            plugin,
            enabled: false
        }));
        assert!(!management.current_package_target(&Request::Remove {
            kind: PluginKind::Native,
            id: "operator".into()
        }));
        assert!(!management.current_package_target(&Request::Install("operator".into())));
        use crikey_ui::settings_management::PackageDetailsTarget;
        assert!(
            management.current_package_target(&Request::Details(PackageDetailsTarget::Local {
                kind: PluginKind::Native,
                id: "operator".into(),
            }))
        );
        assert!(
            !management.current_package_target(&Request::Details(PackageDetailsTarget::Local {
                kind: PluginKind::Legacy,
                id: "operator".into(),
            }))
        );
        assert!(
            !management.current_package_target(&Request::Details(PackageDetailsTarget::Index {
                id: "operator".into(),
            }))
        );
    }

    #[test]
    fn authoritative_empty_packages_withdraw_action_targets_but_failures_do_not() {
        use crikey_app::package_jobs::{JobOutcome, PackageRow};
        use crikey_platform::PluginKind;
        let completed = |rows| JobOutcome::Completed {
            report: Arc::from(""),
            rows,
            restart_required: false,
            details_truncated: false,
        };
        let mut management = Management::new();
        management.observe_package_outcome(&completed(Some(Arc::from([
            PackageRow {
                package_id: "sample".into(),
                plugin: Some(crikey_core::PluginId("native.sample".into())),
                kind: Some(PluginKind::Native),
                name: "Sample".into(),
                version: "1".into(),
                summary: String::new(),
                installed: true,
                enabled: Some(true),
            },
            PackageRow {
                package_id: "available".into(),
                plugin: None,
                kind: Some(PluginKind::Native),
                name: "Available".into(),
                version: "2".into(),
                summary: String::new(),
                installed: false,
                enabled: None,
            },
        ]))));
        assert_eq!(
            management.panel.packages[0].plugin.as_ref().unwrap().0,
            "native.sample"
        );
        assert_eq!(management.panel.packages[0].kind, Some(PluginKind::Native));
        let remove = crikey_ui::settings_management::Request::Remove {
            kind: PluginKind::Native,
            id: "sample".into(),
        };
        let enable = crikey_ui::settings_management::Request::Enable {
            plugin: crikey_core::PluginId("native.sample".into()),
            enabled: false,
        };
        let install = crikey_ui::settings_management::Request::Install("available".into());
        assert!(management.current_package_target(&remove));
        assert!(management.current_package_target(&enable));
        assert!(management.current_package_target(&install));
        let populated_revision = management.panel.packages_revision;
        management.observe_package_outcome(&completed(None));
        management.observe_package_outcome(&JobOutcome::Failed {
            error: JobError::new("inventory-unavailable", "Inventory unavailable"),
            reconciliation: None,
            restart_required: false,
        });
        assert_eq!(
            management.panel.packages.len(),
            2,
            "non-row and failed operations are not inventory authority"
        );
        assert_eq!(management.panel.packages_revision, populated_revision);
        management.observe_package_outcome(&completed(Some(Arc::from([]))));
        assert!(
            management.panel.packages.is_empty(),
            "old remove/enable/install targets must disappear"
        );
        assert!(!management.current_package_target(&remove));
        assert!(!management.current_package_target(&enable));
        assert!(!management.current_package_target(&install));
        assert!(
            management.panel.packages_revision > populated_revision,
            "old destructive confirmations must expire"
        );
    }

    #[test]
    fn shared_launcher_metadata_remains_reachable_by_category_and_deep_link() {
        use crate::command_preferences::PreferenceWorker;
        use crikey_app::CommandPreferencesStore;
        use crikey_ui::settings_management::Category;
        use std::time::{Duration, Instant};

        fn receive_projection(
            management: &mut Management,
            worker: &PreferenceWorker,
            view: crikey_ui::ViewId,
            wakes: &mpsc::Receiver<()>,
        ) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                while worker.take().is_some() {}
                management.drain(Some((view, crikey_ui::ViewKind::Settings)), worker, None);
                if !management.panel.loading {
                    assert!(
                        management.panel.authoritative,
                        "real authority must approve the projection"
                    );
                    return;
                }
                wakes
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("configuration authority must complete the correlated search");
            }
        }

        let directory = ConfigDir::new("canonical-categories");
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
        .fold(DirectoryEnvironment::new(), |environment, variable| {
            environment.set(variable, directory.root.as_os_str())
        });
        let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment)
            .expect("isolated configuration directories");
        let config = ConfigStore::load_with_policy(&directories, None).expect("real configuration");
        let (wake, wakes) = mpsc::channel();
        let mut worker = PreferenceWorker::spawn(
            Arc::new(Mutex::new(CommandPreferencesStore::empty())),
            Some(config),
            directories,
            Vec::new(),
            None,
            Vec::new(),
            move || {
                let _ = wake.send(());
            },
        )
        .expect("real configuration authority");
        let mut model = LauncherViewModel::new();
        model.activate();
        model.open_settings(None);
        let view = model.view().expect("settings view").0;
        let mut management = Management::new();

        // These are the approved user-facing destinations, not replacement
        // metadata: every request and returned field below uses the real shared
        // declarations and the production authority/consumer projection.
        for (key, destination) in [
            (KEY_ACTIVATION_HOTKEY, Category::Keyboard),
            (KEY_MAX_RESULTS, Category::Launcher),
            (crikey_config::KEY_PROFILE, Category::Launcher),
            (KEY_SHOW_HINTS, Category::Appearance),
            (KEY_ROUNDED_CORNERS, Category::Appearance),
            ("launcher.preview-split", Category::Appearance),
            ("launcher.resume-delay-ms", Category::Launcher),
            ("launcher.query-history", Category::PrivacyHistory),
            ("launcher.command-home", Category::Launcher),
            (crikey_config::KEY_THEME, Category::Appearance),
            (crikey_config::KEY_INTERFACE_SIZE, Category::Appearance),
            (crikey_config::KEY_RESULT_DENSITY, Category::Appearance),
            (crikey_config::KEY_THEME_OVERRIDES, Category::Appearance),
            (crikey_config::KEY_NAVIGATION_STYLE, Category::Keyboard),
            (crikey_config::KEY_NAVIGATION_BINDINGS, Category::Keyboard),
        ] {
            let declaration =
                crikey_app::launcher_settings::declaration(key).expect("shared launcher declaration");
            assert!(management.route_focus(view, Some(declaration.navigation_target), &worker));
            receive_projection(&mut management, &worker, view, &wakes);
            assert_eq!(
                management.panel.category, destination,
                "deep-link destination for {key}"
            );
            assert!(
                management.panel.fields.iter().any(|field| {
                    field.key == declaration.key && field.category == management.panel.category.label()
                }),
                "the deep-linked field must survive the selected category filter: {key}"
            );
            let field = management
                .panel
                .fields
                .iter()
                .find(|field| field.key == key)
                .unwrap();
            assert_eq!(field.max_bytes, declaration.max_edit_bytes());
            if let crikey_app::launcher_settings::Control::Choice { options } = declaration.control {
                assert_eq!(field.control, crikey_ui::SettingControl::Choice { options });
            }
            if key == crikey_config::KEY_NAVIGATION_BINDINGS {
                assert_eq!(field.control, crikey_ui::SettingControl::Text);
                assert_eq!(field.max_bytes, 8192);
            }

            management.select_category(view, destination, &worker);
            receive_projection(&mut management, &worker, view, &wakes);
            assert!(
                management.panel.fields.iter().any(|field| {
                    field.key == declaration.key && field.category == management.panel.category.label()
                }),
                "direct category navigation must also expose the shared field: {key}"
            );
        }
        worker
            .take_shutdown()
            .expect("authority shutdown ticket")
            .join()
            .expect("authority shutdown");
    }

    #[test]
    fn optional_plugin_boolean_stays_unset_until_saved_then_refreshes_as_a_toggle() {
        use crate::command_preferences::{PreferenceWorker, SettingsOperation, SettingsTarget};
        use crikey_app::CommandPreferencesStore;
        use std::time::{Duration, Instant};

        fn receive_projection(
            management: &mut Management,
            worker: &PreferenceWorker,
            view: crikey_ui::ViewId,
            wakes: &mpsc::Receiver<()>,
        ) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                while worker.take().is_some() {}
                management.drain(Some((view, crikey_ui::ViewKind::Settings)), worker, None);
                if !management.panel.loading {
                    assert!(management.panel.authoritative);
                    return;
                }
                wakes
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("real settings request and any post-save refresh must complete");
            }
        }

        let directory = ConfigDir::new("optional-plugin-boolean");
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
        .fold(DirectoryEnvironment::new(), |environment, variable| {
            environment.set(variable, directory.root.as_os_str())
        });
        let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment)
            .expect("isolated configuration directories");
        let root = directory.root.join("packages");
        let package = root.join("fixture");
        std::fs::create_dir_all(&package).expect("plugin fixture directory");
        std::fs::write(
            package.join("crikey.toml"),
            concat!(
                include_str!("../../crikey-plugin-model/tests/data/spec-example.crikey.toml"),
                "\n[[configuration.field]]\nname = \"optional\"\ntype = \"boolean\"\n",
            ),
        )
        .expect("optional boolean declaration without a default");
        let plugin = crikey_core::PluginId("native.dev.example.repositories".into());
        let key = format!("plugins.{}.settings.optional", plugin.0);
        let config = ConfigStore::load_with_policy(&directories, None).expect("real configuration");
        assert_eq!(config.get(&key), None);
        let (wake, wakes) = mpsc::channel();
        let mut worker = PreferenceWorker::spawn(
            Arc::new(Mutex::new(CommandPreferencesStore::empty())),
            Some(config),
            directories,
            Vec::new(),
            None,
            vec![root],
            move || {
                let _ = wake.send(());
            },
        )
        .expect("real configuration authority");
        let mut model = LauncherViewModel::new();
        model.activate();
        model.open_settings(None);
        let view = model.view().expect("settings view").0;
        let mut management = Management::new();
        management.search(
            view,
            SettingsOperation::Search { text: String::new() },
            &worker,
            None,
        );
        receive_projection(&mut management, &worker, view, &wakes);
        let field = management
            .panel
            .fields
            .iter()
            .find(|field| field.key == key)
            .unwrap();
        assert!(field.value_available);
        assert_eq!(field.plugin.as_ref(), Some(&plugin));
        assert_eq!(field.value, "");
        assert_eq!(field.source, "unset");
        assert_eq!(
            field.control,
            crikey_ui::SettingControl::Text,
            "unset must expose the existing text editor and Save"
        );
        assert_eq!(
            directory.store().get(&key),
            None,
            "viewing must not supply a value"
        );
        let launcher = management
            .panel
            .fields
            .iter()
            .find(|field| field.key == KEY_ROUNDED_CORNERS)
            .unwrap();
        assert_eq!(
            launcher.control,
            crikey_ui::SettingControl::Toggle { on: true },
            "launcher defaults remain checkboxes"
        );

        // The first text edit can set true, rather than first having to uncheck
        // a fabricated true value. The production save response triggers another
        // authority search, which replaces the text editor with a checkbox.
        for (setting, target, values) in [
            (key.as_str(), Some(plugin.clone()), [true, false, true]),
            (KEY_ROUNDED_CORNERS, None, [false, true, false]),
        ] {
            for value in values {
                management.search(
                    view,
                    SettingsOperation::Edit {
                        key: setting.to_owned(),
                        target: target.clone().map_or(SettingsTarget::UserGlobal, |plugin| {
                            SettingsTarget::PluginUser { plugin }
                        }),
                        draft: value.to_string(),
                    },
                    &worker,
                    Some((management.panel.config, management.panel.schema)),
                );
                receive_projection(&mut management, &worker, view, &wakes);
                assert!(management.panel.edit_notice.starts_with("Saved."));
                let field = management
                    .panel
                    .fields
                    .iter()
                    .find(|field| field.key == setting)
                    .unwrap();
                assert_eq!(field.value, value.to_string());
                assert_eq!(field.control, crikey_ui::SettingControl::Toggle { on: value });
                assert_eq!(
                    field.source,
                    if target.is_some() {
                        "user-plugin"
                    } else {
                        "user-global"
                    }
                );
                assert_eq!(directory.store().get(setting), Some(value.to_string().as_str()));
            }
        }
        worker
            .take_shutdown()
            .expect("authority shutdown ticket")
            .join()
            .expect("authority shutdown");
    }

    /// A private configuration directory, removed on drop, so a test writes a
    /// real `config.toml` through the real store rather than through a stub.
    struct ConfigDir {
        root: std::path::PathBuf,
    }

    impl ConfigDir {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "crikey-settings-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("a temporary directory can be created");
            Self { root }
        }

        fn store(&self) -> ConfigStore {
            // Every variable any convention consults, pinned to one scratch
            // directory: the Windows base is resolved from `APPDATA` and
            // `LOCALAPPDATA` before the `CRIKEY_*_DIR` overrides are applied,
            // so a fixture that pins only the overrides fails on Windows with
            // a missing variable rather than testing anything about settings.
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
            .fold(DirectoryEnvironment::new(), |environment, variable| {
                environment.set(variable, self.root.as_os_str())
            });
            // The host's own convention, not a fixed one: `temp_dir()` answers
            // `C:\...` on Windows, which the XDG rule rightly refuses as not
            // absolute, and the fixture would fail for a reason that has
            // nothing to do with settings.
            let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment)
                .expect("every directory is pinned by an override");
            ConfigStore::load_with_policy(&directories, None).expect("an empty tree loads")
        }
    }

    impl Drop for ConfigDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn terminal_appearance_and_navigation_edits_reject_full_candidate_without_writes() {
        let directory = ConfigDir::new("appearance-navigation-validation");
        let mut store = directory.store();
        persist(&mut store, crikey_config::KEY_THEME, "dark").unwrap();
        let before = std::fs::read(store.config_path()).unwrap();
        let revision = store.revision();
        for (key, value, reason) in [
            (
                crikey_config::KEY_THEME_OVERRIDES,
                r##"{"canvas":"#111111","text":"#111111"}"##,
                "contrast",
            ),
            (
                crikey_config::KEY_NAVIGATION_BINDINGS,
                r#"{"next":["Ctrl+Alt+J"],"previous":["Ctrl+Alt+J"]}"#,
                "overlap",
            ),
            (KEY_ACTIVATION_HOTKEY, "Down", "navigation"),
        ] {
            let error = persist(&mut store, key, value).unwrap_err();
            assert!(error.to_lowercase().contains(reason), "{error}");
            assert_eq!(std::fs::read(store.config_path()).unwrap(), before);
            assert_eq!(store.revision(), revision);
        }
    }

    #[test]
    fn terminal_invalid_chord_diagnostics_never_include_unknown_or_trailing_components() {
        let directory = ConfigDir::new("navigation-redacted-diagnostics");
        let mut store = directory.store();
        persist(&mut store, crikey_config::KEY_THEME, "dark").unwrap();
        let before = std::fs::read(store.config_path()).unwrap();
        for chord in ["Ctrl+PRIVATE_CHORD_CANARY", "Ctrl+J+PRIVATE_CHORD_CANARY"] {
            let draft = format!(r#"{{"next":["{chord}"]}}"#);
            let error = persist(&mut store, crikey_config::KEY_NAVIGATION_BINDINGS, &draft).unwrap_err();
            assert!(error.contains("known"), "{error}");
            assert!(!error.contains("PRIVATE_CHORD_CANARY"), "{error}");
            assert_eq!(std::fs::read(store.config_path()).unwrap(), before);
        }
    }

    #[test]
    fn a_dismiss_hides_the_launcher_and_a_later_activation_shows_it_again() {
        // The defect this closes: dismissing used to end the process whenever
        // the hotkey had not registered, so the owner's only route back to the
        // launcher was a terminal.
        let mut view_model = LauncherViewModel::new();
        view_model.activate();
        let effect = view_model
            .apply(UiCommand::Cancel)
            .expect("cancelling a launcher with an empty query dismisses it");
        assert_eq!(effect, UiEffect::Dismissed);
        assert_eq!(residency(&effect), Some(Residency::Hide));

        view_model.activate();
        assert!(
            view_model.is_visible(),
            "the process survived the dismiss, so the same launcher can be raised again"
        );
    }

    #[test]
    fn only_a_quit_ends_the_process() {
        assert_eq!(residency(&UiEffect::Quit), Some(Residency::Exit));
        assert_eq!(residency(&UiEffect::Query("x".to_owned())), None);
        assert_eq!(
            residency(&UiEffect::SetSetting {
                request_id: crikey_ui::next_setting_edit_id(),
                key: KEY_MAX_RESULTS.to_owned(),
                value: "5".to_owned(),
            }),
            None
        );
    }

    #[test]
    fn a_refused_hotkey_leaves_the_launcher_running_and_opens_the_row_that_fixes_it() {
        let mut view_model = LauncherViewModel::new();
        let diagnostic = surface_hotkey_failure(&mut view_model, "Ctrl+Alt+Space", "already owned");
        assert!(diagnostic.contains("Ctrl+Alt+Space"), "{diagnostic}");
        assert!(diagnostic.contains("already owned"), "{diagnostic}");
        assert!(diagnostic.contains("stays running"), "{diagnostic}");

        view_model.activate();
        let frame = view_model.frame().expect("the launcher comes up visible");
        assert!(frame.settings_open, "the user needs somewhere to fix it");
        assert_eq!(frame.settings_focus.as_deref(), Some(KEY_ACTIVATION_HOTKEY));

        // And it is still resident: an Escape hides rather than exits.
        assert_eq!(residency(&UiEffect::Dismissed), Some(Residency::Hide));
    }

    #[test]
    fn setting_the_activation_hotkey_persists_to_the_user_global_layer() {
        let directory = ConfigDir::new("hotkey-set");
        let mut store = directory.store();
        let report = persist(&mut store, KEY_ACTIVATION_HOTKEY, "Ctrl+Shift+P").unwrap();
        assert_eq!(report, "launcher.activation-hotkey = Ctrl+Shift+P");
        let reloaded = directory.store();
        assert_eq!(reloaded.get(KEY_ACTIVATION_HOTKEY), Some("Ctrl+Shift+P"));
        assert_eq!(
            reloaded.layer_of(KEY_ACTIVATION_HOTKEY),
            Some(ConfigLayer::UserGlobal)
        );
    }

    #[test]
    fn an_unparseable_accelerator_never_reaches_the_configuration_file() {
        let directory = ConfigDir::new("hotkey-garbage");
        let mut store = directory.store();

        let report = persist(&mut store, KEY_ACTIVATION_HOTKEY, "Ctrl+").unwrap_err();

        assert!(report.contains("not a valid accelerator"), "{report}");
        let reloaded = directory.store();
        assert_eq!(
            reloaded.get(KEY_ACTIVATION_HOTKEY),
            Some("Ctrl+Alt+Space"),
            "the refused edit left the built-in default in place"
        );
    }

    #[test]
    fn a_setting_a_higher_layer_outranks_is_saved_and_reported_as_outranked() {
        let directory = ConfigDir::new("outranked");
        std::fs::write(
            directory.root.join("config.toml"),
            "[launcher]\nprofile = \"work\"\n",
        )
        .expect("the user file can be written");
        std::fs::create_dir_all(directory.root.join("profiles")).expect("profiles directory");
        std::fs::write(
            directory.root.join("profiles").join("work.toml"),
            "[launcher]\nactivation-hotkey = \"Ctrl+Alt+K\"\n",
        )
        .expect("the profile can be written");
        let mut store = directory.store();

        let report = persist(&mut store, KEY_ACTIVATION_HOTKEY, "Ctrl+Shift+P").unwrap();

        assert!(report.contains("still supplies Ctrl+Alt+K"), "{report}");
        assert_eq!(
            configured_hotkey(Some(&store)),
            "Ctrl+Alt+K",
            "registration consumes the complete effective state, not the submitted draft"
        );
    }

    /// The hint line's setting has to survive the whole way round: the panel
    /// offers it, a write lands in the layer `crikey config` reports, and the
    /// panel then reads back what was written. A key the renderer honours but
    /// the surface never lists is a setting only someone who read the source
    /// could find.
    #[test]
    fn the_hint_line_setting_round_trips_through_the_settings_rows() {
        let directory = ConfigDir::new("show-hints");
        let mut store = directory.store();

        let default = rows(Some(&store))
            .into_iter()
            .find(|row| row.key == KEY_SHOW_HINTS)
            .expect("the panel offers the hint line");
        assert_eq!(default.label, "Show keyboard hints");
        assert_eq!(default.value, "true", "an unconfigured launcher shows its hints");
        assert_eq!(default.source, ConfigLayer::BuiltInDefaults.as_str());
        assert!(configured_show_hints(Some(&store)));

        let report = persist(&mut store, KEY_SHOW_HINTS, "false").expect("the key is a setting");
        assert_eq!(report, "launcher.show-hints = false");

        let reloaded = directory.store();
        let saved = rows(Some(&reloaded))
            .into_iter()
            .find(|row| row.key == KEY_SHOW_HINTS)
            .expect("the panel still offers the hint line");
        assert_eq!(saved.value, "false");
        assert_eq!(saved.source, ConfigLayer::UserGlobal.as_str());
        assert!(
            !configured_show_hints(Some(&reloaded)),
            "the launcher reads back the choice the panel wrote"
        );
    }

    /// The renderer takes this as a plain boolean, so a value it cannot read is
    /// refused on the way in rather than silently treated as "off" -- a user who
    /// typed `yes` would otherwise be looking at a hidden hint line and a
    /// configuration file that says they asked for something else.
    #[test]
    fn a_hint_line_value_that_is_not_a_boolean_is_refused_before_it_is_written() {
        for accepted in ["true", "false"] {
            assert!(
                validate(KEY_SHOW_HINTS, accepted).is_ok(),
                "{accepted} is one of the two values the renderer honours"
            );
        }

        for refused in ["yes", "no", "1", "0", "True", "off"] {
            let error = validate(KEY_SHOW_HINTS, refused).expect_err("only `true` and `false` are honoured");
            assert!(error.contains("must be `true` or `false`"), "{error}");
            assert!(error.contains(refused), "the message names the value: {error}");
        }

        let directory = ConfigDir::new("show-hints-refused");
        let mut store = directory.store();
        assert!(persist(&mut store, KEY_SHOW_HINTS, "yes").is_err());
        assert!(
            configured_show_hints(Some(&directory.store())),
            "a refused edit leaves the launcher showing what it showed before"
        );
    }

    /// The corner setting has to survive the whole way round for the same
    /// reason the hint line does: the panel offers it, the write lands in the
    /// layer `crikey config` reports, and the panel reads back what was
    /// written. A key the renderer honours but the surface never lists is a
    /// setting only someone who read the source could find.
    #[test]
    fn the_rounded_corner_setting_round_trips_through_the_settings_rows() {
        let directory = ConfigDir::new("rounded-corners");
        let mut store = directory.store();

        let default = rows(Some(&store))
            .into_iter()
            .find(|row| row.key == KEY_ROUNDED_CORNERS)
            .expect("the panel offers the window's corners");
        assert_eq!(default.label, "Rounded corners");
        assert_eq!(
            default.value, "true",
            "an unconfigured launcher rounds its corners"
        );
        assert_eq!(default.source, ConfigLayer::BuiltInDefaults.as_str());
        assert!(configured_rounded_corners(Some(&store)));

        let report = persist(&mut store, KEY_ROUNDED_CORNERS, "false").expect("the key is a setting");
        assert_eq!(report, "launcher.rounded-corners = false");

        let reloaded = directory.store();
        let saved = rows(Some(&reloaded))
            .into_iter()
            .find(|row| row.key == KEY_ROUNDED_CORNERS)
            .expect("the panel still offers the window's corners");
        assert_eq!(saved.value, "false");
        assert_eq!(saved.source, ConfigLayer::UserGlobal.as_str());
        assert!(
            !configured_rounded_corners(Some(&reloaded)),
            "the launcher reads back the choice the panel wrote"
        );
    }

    /// The window shape is a plain boolean, so a value the renderer cannot read
    /// is refused on the way in rather than silently squaring the corners off:
    /// the shape of the window is the only report of this setting, so a user who
    /// typed `yes` would have nothing on screen naming the value at fault.
    #[test]
    fn a_rounded_corner_value_that_is_not_a_boolean_is_refused_before_it_is_written() {
        for accepted in ["true", "false"] {
            assert!(
                validate(KEY_ROUNDED_CORNERS, accepted).is_ok(),
                "{accepted} is one of the two values the renderer honours"
            );
        }

        for refused in ["yes", "no", "1", "0", "True", "off"] {
            let error =
                validate(KEY_ROUNDED_CORNERS, refused).expect_err("only `true` and `false` are honoured");
            assert!(error.contains("must be `true` or `false`"), "{error}");
            assert!(error.contains(refused), "the message names the value: {error}");
        }

        let directory = ConfigDir::new("rounded-corners-refused");
        let mut store = directory.store();
        assert!(persist(&mut store, KEY_ROUNDED_CORNERS, "yes").is_err());
        assert!(
            configured_rounded_corners(Some(&directory.store())),
            "a refused edit leaves the window the shape it already had"
        );
    }

    /// A hand-edited configuration file is the one place an unrecognised value
    /// can reach the reader, because [`validate`] never saw it. Only the exact
    /// text `false` may square the corners off: any other text is a typo, and
    /// changing the shape of the window over one would leave the user with no
    /// way to tell a rejected edit from an applied one.
    #[test]
    fn only_the_exact_text_false_squares_the_launchers_corners_off() {
        let directory = ConfigDir::new("rounded-corners-lenient");

        assert!(
            configured_rounded_corners(None),
            "a launch whose configuration would not load still draws a window"
        );

        let mut store = directory.store();
        assert!(persist(&mut store, KEY_ROUNDED_CORNERS, "false").is_ok());
        assert!(!configured_rounded_corners(Some(&directory.store())));

        for typo in ["False", "FALSE", "no", "0", "off", " false"] {
            let mut store = directory.store();
            store.set_user_global(KEY_ROUNDED_CORNERS, typo);
            assert!(
                configured_rounded_corners(Some(&store)),
                "`{typo}` is not `false`, so the corners stay round"
            );
        }
    }

    /// The command line and the panel must be one settings surface, not two:
    /// a value written here is the value the panel then shows, and an unknown
    /// key is refused rather than stored where nothing will ever read it.
    #[test]
    fn a_command_line_write_lands_in_the_same_layer_the_panel_writes() {
        let directory = ConfigDir::new("cli-write");
        let mut store = directory.store();

        let report = persist(&mut store, KEY_ACTIVATION_HOTKEY, "Ctrl+Alt+K").expect("the key is a setting");
        assert_eq!(report, "launcher.activation-hotkey = Ctrl+Alt+K");
        assert_eq!(
            store.layer_of(KEY_ACTIVATION_HOTKEY),
            Some(ConfigLayer::UserGlobal)
        );

        let reloaded = directory.store();
        let row = rows(Some(&reloaded))
            .into_iter()
            .find(|row| row.key == KEY_ACTIVATION_HOTKEY)
            .expect("the panel offers the hotkey");
        assert_eq!(
            row.value, "Ctrl+Alt+K",
            "the panel reads back what the command line wrote"
        );
        assert_eq!(row.source, ConfigLayer::UserGlobal.as_str());

        let refused = persist(&mut store, "launcher.nope", "x").expect_err("an unknown key is not a setting");
        assert!(refused.contains("is not a launcher setting"), "{refused}");
    }
}
