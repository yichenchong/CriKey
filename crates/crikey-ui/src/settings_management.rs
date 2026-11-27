//! Host-correlated settings and process-scoped package presentation.
use crate::{UiCommand, ViewId};
use crikey_core::PluginId;
use crikey_platform::PluginKind;
use std::sync::Arc;

#[derive(Clone, PartialEq, Eq)]
pub enum Request {
    BrowsePlugin(PluginId),
    LegacyConfiguration(String),
    Search(String),
    Category(Category),
    Edit {
        request_id: u64,
        key: String,
        plugin: Option<PluginId>,
        draft: String,
        config: u64,
        schema: u64,
    },
    Packages,
    PackageSearch(String),
    Details(PackageDetailsTarget),
    UpdateIndex,
    Install(String),
    InstallFrom {
        source: String,
        kind: SourceKind,
    },
    Remove {
        kind: PluginKind,
        id: String,
    },
    Enable {
        plugin: PluginId,
        enabled: bool,
    },
    Diagnose(Option<String>),
    Cancel(u64),
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SettingsRequest([private])")
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageDetailsTarget {
    Index { id: String },
    Local { kind: PluginKind, id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Index,
    Directory,
    Archive,
    Legacy,
    Url,
}
impl SourceKind {
    fn label(self) -> &'static str {
        match self {
            Self::Index => "Index ID",
            Self::Directory => "Package directory",
            Self::Archive => "Package archive",
            Self::Legacy => "Legacy package",
            Self::Url => "HTTPS package URL",
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuntimeState {
    #[default]
    Open,
    Closing,
    Offline,
    FailedClosed,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Category {
    #[default]
    All,
    Launcher,
    Appearance,
    Keyboard,
    Shortcuts,
    Plugins,
    PrivacyHistory,
}
impl Category {
    pub const ALL: [Self; 7] = [
        Self::All,
        Self::Launcher,
        Self::Appearance,
        Self::Keyboard,
        Self::Shortcuts,
        Self::Plugins,
        Self::PrivacyHistory,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Launcher => "Launcher",
            Self::Appearance => "Appearance",
            Self::Keyboard => "Keyboard",
            Self::Shortcuts => "Shortcuts",
            Self::Plugins => "Plugins",
            Self::PrivacyHistory => "Privacy/History",
        }
    }
    pub fn from_metadata(label: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|category| *category != Self::All && category.label() == label)
    }
    pub fn shows(self, section: Self) -> bool {
        self == Self::All || self == section
    }
    fn includes(self, category: &str) -> bool {
        self == Self::All || self.label() == category
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Panel {
    pub view: Option<ViewId>,
    pub category: Category,
    pub query_reset: u64,
    pub request: u64,
    pub config: u64,
    pub schema: u64,
    pub projection_config: u64,
    pub projection_schema: u64,
    pub authoritative: bool,
    /// Correlated typed schema-refusal receipt; persists across fresh projections.
    pub schema_refusal: u64,
    pub schema_rejected: bool,
    pub edit_notice: Arc<str>,
    pub loading: bool,
    pub notice: Arc<str>,
    pub fields: Arc<[Field]>,
    pub packages: Arc<[Package]>,
    pub jobs: Arc<[Job]>,
    pub packages_revision: u64,
    pub acknowledged: Option<(String, u64)>,
    pub activation: Arc<str>,
    pub runtime_state: RuntimeState,
}
#[derive(Clone, PartialEq, Eq)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub description: String,
    pub category: String,
    pub plugin: Option<PluginId>,
    pub value: String,
    pub source: String,
    pub restart: bool,
    pub secret: bool,
    pub toggle: Option<bool>,
    pub value_available: bool,
}
impl std::fmt::Debug for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingField")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub id: String,
    pub plugin: Option<PluginId>,
    pub kind: Option<PluginKind>,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub installed: bool,
    pub enabled: Option<bool>,
}

fn package_diagnosis_request(package: &Package) -> Request {
    Request::Diagnose(Some(
        package
            .plugin
            .as_ref()
            .map_or_else(|| package.id.clone(), |plugin| plugin.0.clone()),
    ))
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: u64,
    pub sequence: u64,
    pub operation: &'static str,
    pub status: Arc<str>,
    pub terminal: bool,
}
#[derive(Clone, Default)]
struct Draft {
    text: String,
    config: u64,
    schema: u64,
    schema_refusal: u64,
    changed: bool,
    revision: u64,
    pending: Option<(u64, u64)>,
    focused: bool,
}

#[derive(Clone, Copy)]
struct EditorIdentity {
    id: egui::Id,
    secret: bool,
    schema: u64,
}
#[derive(Clone, Default)]
struct Editors {
    view: Option<ViewId>,
    entries: Vec<EditorIdentity>,
    schema_refusal: u64,
    category: Category,
    query_reset: u64,
}

fn editor_id(view: Option<ViewId>, key: &str, secret: bool) -> egui::Id {
    egui::Id::new(("managed-setting", view.map(ViewId::get), key, secret))
}
fn retire_text_state(context: &egui::Context, id: egui::Id) {
    let private_focus = context.data_mut(|data| {
        data.remove::<egui::text_edit::TextEditState>(id.with("text"));
        let focus = data.get_temp::<egui::Id>(id.with("private-focus"));
        data.remove::<egui::Id>(id.with("private-focus"));
        focus
    });
    context.memory_mut(|memory| {
        memory.surrender_focus(id.with("text"));
        if let Some(focus) = private_focus {
            memory.surrender_focus(focus);
        }
    });
}
fn retire_projection(context: &egui::Context, panel: &Panel, open: bool) {
    let registry = egui::Id::new("managed-settings-editor-registry");
    let mut editors = context
        .data_mut(|data| data.remove_temp::<Editors>(registry))
        .unwrap_or_default();
    let same_view = open && editors.view == panel.view;
    let category_changed = editors.category != panel.category;
    let private_revoked = editors.schema_refusal != panel.schema_refusal || category_changed;
    if same_view && (category_changed || editors.query_reset != panel.query_reset) {
        context.data_mut(|data| {
            data.remove::<String>(egui::Id::new((
                "settings-metadata-search",
                panel.view.map(ViewId::get),
            )))
        });
    }
    editors.entries.retain(|editor| {
        let visible = same_view
            && panel.fields.iter().any(|field| {
                panel.category.includes(&field.category)
                    && editor_id(panel.view, &field.key, field.secret) == editor.id
            });
        if visible
            && (!editor.secret || editor.schema == panel.schema && !private_revoked && !panel.schema_rejected)
        {
            return true;
        }
        // Filtering is not closing the logical settings view. Retain bounded
        // nonsecret drafts with their original config/schema stamps so returning
        // to a field still requires explicit rebase after a conflict.
        let draft = context.data_mut(|data| data.remove_temp::<Draft>(editor.id));
        retire_text_state(context, editor.id);
        if same_view && !editor.secret {
            if let Some(draft) = draft.filter(|draft| draft.changed || draft.pending.is_some()) {
                context.data_mut(|data| data.insert_temp(editor.id, draft));
                return true;
            }
        }
        false
    });
    if !same_view {
        context.data_mut(|data| {
            data.remove::<(u64, Request)>(egui::Id::new("package-confirm"));
            data.remove::<String>(egui::Id::new("package-source"));
            data.remove::<String>(egui::Id::new((
                "settings-metadata-search",
                editors.view.map(ViewId::get),
            )));
        });
    }
    editors.view = if open { panel.view } else { None };
    editors.schema_refusal = panel.schema_refusal;
    editors.category = panel.category;
    editors.query_reset = panel.query_reset;
    context.data_mut(|data| data.insert_temp(registry, editors));
}
pub(crate) fn clear_editors(context: &egui::Context) {
    retire_projection(context, &Panel::default(), false);
}
pub(crate) fn retire_editors(context: &egui::Context, model: &crate::ViewModel) {
    retire_projection(
        context,
        &model.navigation.settings_management,
        model.settings_open,
    );
}

/// Identifies the live focused control without retaining any editor contents.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct FocusOwner {
    id: egui::Id,
    category: Category,
    query_reset: u64,
    field: Option<(String, bool, u64)>,
}

pub(crate) fn focused_owner(context: &egui::Context, panel: &Panel) -> Option<FocusOwner> {
    let id = context.memory(|memory| memory.focused())?;
    let field = panel.fields.iter().find(|field| {
        let editor = editor_id(panel.view, &field.key, field.secret);
        id == editor.with("text")
            || context.data(|data| data.get_temp::<egui::Id>(editor.with("private-focus"))) == Some(id)
    }).map(|field| (field.key.clone(), field.secret, if field.secret { panel.schema } else { 0 }));
    Some(FocusOwner { id, category: panel.category, query_reset: panel.query_reset, field })
}
fn draw_input(
    ui: &mut egui::Ui,
    id: egui::Id,
    secret: bool,
    text: &mut String,
) -> (egui::Response, bool, bool) {
    let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
    if !secret {
        let response = ui.add(
            egui::TextEdit::singleline(text)
                .id(id.with("text"))
                .char_limit(4096),
        );
        let changed = response.changed();
        let commit = enter && response.lost_focus();
        return (response, changed, commit);
    }
    // As in the structured editor, plaintext never enters TextEdit/undo state.
    ui.data_mut(|data| data.remove::<egui::text_edit::TextEditState>(id.with("text")));
    let response = ui
        .push_id(id.with("private"), |ui| {
            ui.add(
                egui::Button::new(if text.is_empty() {
                    "Enter secret"
                } else {
                    "••••••••"
                })
                .sense(egui::Sense::click()),
            )
        })
        .inner;
    ui.data_mut(|data| data.insert_temp(id.with("private-focus"), response.id));
    if response.clicked() {
        response.request_focus();
    }
    let mut changed = false;
    if response.has_focus() {
        ui.ctx().output_mut(|output| {
            output.ime = Some(egui::output::IMEOutput {
                rect: response.rect,
                cursor_rect: response.rect,
            })
        });
        ui.input_mut(|input| {
            input.events.retain(|event| match event {
                egui::Event::Text(value)
                | egui::Event::Paste(value)
                | egui::Event::Ime(egui::ImeEvent::Commit(value)) => {
                    if text
                        .len()
                        .checked_add(value.len())
                        .is_some_and(|bytes| bytes <= 4096)
                    {
                        text.push_str(value);
                        changed |= !value.is_empty();
                    }
                    false
                }
                egui::Event::Key {
                    key: egui::Key::Backspace,
                    pressed: true,
                    ..
                } => {
                    changed |= text.pop().is_some();
                    false
                }
                egui::Event::Ime(_) => false,
                _ => true,
            })
        });
    }
    let commit = enter && response.has_focus();
    (response, changed, commit)
}
pub(crate) fn draw_categories(ui: &mut egui::Ui, panel: &Panel, commands: &mut Vec<UiCommand>) {
    ui.horizontal_wrapped(|ui| {
        for category in Category::ALL {
            if ui
                .selectable_label(panel.category == category, category.label())
                .clicked()
            {
                commands.push(UiCommand::ManageSettings(Request::Category(category)));
            }
        }
    });
}

fn draw_job(ui: &mut egui::Ui, job: &Job, commands: &mut Vec<UiCommand>) {
    ui.group(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.strong(job.operation);
            if !job.terminal && ui.button("Request cancellation").clicked() {
                commands.push(UiCommand::ManageSettings(Request::Cancel(job.id)));
            }
        });
        // Reports include security findings and unbroken paths/identities.
        // Wrap every character if necessary; never elide or truncate findings.
        let mut text = egui::text::LayoutJob::simple(
            job.status.to_string(),
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
            ui.available_width().max(1.0),
        );
        text.wrap.break_anywhere = true;
        ui.add(egui::Label::new(text).wrap().selectable(true));
    });
}

pub(crate) fn draw(
    ui: &mut egui::Ui,
    panel: &Panel,
    outcomes: &[crate::SettingEditOutcome],
    focus: Option<&str>,
    commands: &mut Vec<UiCommand>,
) {
    let send = |request, commands: &mut Vec<UiCommand>| commands.push(UiCommand::ManageSettings(request));
    let search_id = egui::Id::new(("settings-metadata-search", panel.view.map(ViewId::get)));
    let mut query = ui
        .data_mut(|data| data.remove_temp::<String>(search_id))
        .unwrap_or_default();
    if ui
        .add(
            egui::TextEdit::singleline(&mut query)
                .id(search_id)
                .char_limit(4096)
                .hint_text("Search settings by name, description, or plugin"),
        )
        .changed()
    {
        send(Request::Search(query.clone()), commands);
    }
    if panel.loading {
        ui.spinner();
        ui.label("Loading settings…");
    }
    if ui.button("Refresh settings").clicked() {
        send(Request::Search(query.clone()), commands);
    }
    ui.data_mut(|data| data.insert_temp(search_id, query));
    if !panel.activation.is_empty() {
        ui.small(format!(
            "Activation shortcut currently held: {}",
            panel.activation
        ));
    }
    if !panel.notice.is_empty() {
        ui.label(panel.notice.as_ref());
    }
    if !panel.edit_notice.is_empty() {
        ui.label(panel.edit_notice.as_ref());
    }
    match panel.runtime_state {
        RuntimeState::Open => (),
        RuntimeState::Closing => {
            ui.label(
                "Third-party runtime shutdown/reconciliation is in progress. Built-ins remain available.",
            );
        }
        RuntimeState::Offline => {
            ui.label("Third-party runtimes are offline until the next launch. Built-ins, Settings and activation remain available.");
        }
        RuntimeState::FailedClosed => {
            ui.label(
                "Third-party runtimes are failed-closed. Further package mutation is refused until restart.",
            );
        }
    }
    let projection_ready = panel.authoritative
        && panel.projection_config == panel.config
        && panel.projection_schema == panel.schema;
    let projection_ready = projection_ready && !panel.schema_rejected;
    egui::ScrollArea::vertical().id_salt("settings-fields").max_height(320.0).show(ui, |ui| {
        for field in panel.fields.iter().filter(|field| panel.category.includes(&field.category)) {
            let id = editor_id(panel.view, &field.key, field.secret);
            if field.secret && panel.schema_rejected {
                retire_text_state(ui.ctx(), id);
                ui.group(|ui| {
                    ui.label(&field.label);
                    ui.label("Secret declaration unavailable. Refresh settings before entering a replacement.");
                });
                continue;
            }
            let registry = egui::Id::new("managed-settings-editor-registry");
            let admitted = ui.data_mut(|data| {
                let mut editors = data.remove_temp::<Editors>(registry).unwrap_or_default();
                if !editors.entries.iter().any(|editor| editor.id == id) {
                    if editors.entries.len() == 128 {
                        let Some(clean) = editors.entries.iter().position(|editor| {
                            let Some(draft) = data.remove_temp::<Draft>(editor.id) else { return true; };
                            let dirty = draft.changed || draft.pending.is_some();
                            data.insert_temp(editor.id, draft);
                            !dirty
                        }) else { data.insert_temp(registry, editors); return false; };
                        let retired = editors.entries.remove(clean).id;
                        data.remove::<Draft>(retired);
                        data.remove::<egui::text_edit::TextEditState>(retired.with("text"));
                        data.remove::<egui::Id>(retired.with("private-focus"));
                    }
                    editors.entries.push(EditorIdentity { id, secret: field.secret, schema: panel.schema });
                }
                data.insert_temp(registry, editors);
                true
            });
            if !admitted {
                ui.label(format!("{} — editor capacity reached; finish/discard a draft or close Settings", field.label));
                continue;
            }
            let mut draft = ui.data_mut(|data| data.remove_temp::<Draft>(id)).unwrap_or_else(|| Draft { text: if field.secret { String::new() } else { field.value.clone() }, config: panel.config, schema: panel.schema, schema_refusal: panel.schema_refusal, ..Default::default() });
            // Reuse the existing recorder's consumed-receipt identity across
            // bootstrap and searchable forms; discarding a draft cannot replay it.
            let capture_id = egui::Id::new(("crikey-setting-draft", field.key.as_str())).with("state").with("capture");
            let captured_before = ui.data(|data| data.get_temp::<u64>(capture_id)).unwrap_or(0);
            if let Some(captured) = outcomes.iter().find(|outcome| outcome.key.strip_prefix("capture:") == Some(field.key.as_str()) && outcome.request_id > captured_before) {
                draft.text.clone_from(&captured.value);
                ui.data_mut(|data| data.insert_temp(capture_id, captured.request_id));
                draft.changed = true;
                draft.focused = false;
                draft.revision = draft.revision.checked_add(1).expect("setting draft revision exhausted");
            }
            if let (Some((key, acknowledged)), Some((pending, revision))) = (&panel.acknowledged, draft.pending) {
                if key == &field.key && *acknowledged == pending {
                    retire_text_state(ui.ctx(), id);
                    if revision == draft.revision { draft.changed = false; if field.secret { draft.text.clear(); } }
                    draft.pending = None;
                }
            }
            if !draft.changed && (draft.config != panel.config || draft.schema != panel.schema || draft.schema_refusal != panel.schema_refusal) { draft.config = panel.config; draft.schema = panel.schema; draft.schema_refusal = panel.schema_refusal; if !field.secret { draft.text.clone_from(&field.value); } }
            ui.push_id(id, |ui| ui.group(|ui| {
                ui.label(&field.label);
                ui.small(&field.description);
                ui.small(format!("{} · {}{}", field.key, field.source, if field.restart { " · Next launch required" } else { "" }));
                if !field.value_available {
                    ui.label("Effective value exceeds the display limit. This field is not editable in the graphical form.");
                    return;
                }
                if let Some(on) = field.toggle {
                    let mut value = on;
                    if ui.add_enabled(!panel.loading && projection_ready, egui::Checkbox::new(&mut value, "Enabled")).changed() {
                        send(Request::Edit { request_id: crate::next_setting_edit_id(), key: field.key.clone(), plugin: field.plugin.clone(), draft: value.to_string(), config: panel.config, schema: panel.schema }, commands);
                    }
                    return;
                }
                if field.key == "launcher.activation-hotkey" && ui.button("Record shortcut").clicked() {
                    commands.push(UiCommand::BeginShortcutCapture(field.key.clone()));
                }
                let (response, changed, commit) = ui.add_enabled_ui(!panel.schema_rejected, |ui| draw_input(ui, id, field.secret, &mut draft.text)).inner;
                if focus == Some(field.key.as_str()) && !draft.focused {
                    response.request_focus();
                    draft.focused = true;
                }
                if changed {
                    draft.changed = true;
                    draft.revision = draft.revision.checked_add(1).expect("settings draft revision exhausted");
                }
                let conflict = draft.changed && (draft.config != panel.config || draft.schema != panel.schema || draft.schema_refusal != panel.schema_refusal);
                if conflict { ui.label("Configuration changed. Your draft is retained; explicitly rebase before saving."); }
                ui.horizontal(|ui| {
                    let can_save = draft.changed && !conflict && !panel.loading && projection_ready;
                    if (ui.add_enabled(can_save, egui::Button::new("Save")).clicked() || commit) && can_save {
                        retire_text_state(ui.ctx(), id);
                        let request_id = crate::next_setting_edit_id();
                        draft.pending = Some((request_id, draft.revision));
                        send(Request::Edit { request_id, key: field.key.clone(), plugin: field.plugin.clone(), draft: draft.text.clone(), config: draft.config, schema: draft.schema }, commands);
                    }
                    if conflict && ui.add_enabled(projection_ready, egui::Button::new("Rebase draft")).clicked() { draft.config = panel.config; draft.schema = panel.schema; draft.schema_refusal = panel.schema_refusal; }
                    if draft.changed && ui.button("Discard draft").clicked() {
                        retire_text_state(ui.ctx(), id);
                        draft = Draft { text: if field.secret { String::new() } else { field.value.clone() }, config: panel.config, schema: panel.schema, schema_refusal: panel.schema_refusal, ..Default::default() };
                    }
                });
            }));
            ui.data_mut(|data| data.insert_temp(id, draft));
        }
    });
    if panel.category.shows(Category::Plugins) {
        let mut packages = |ui: &mut egui::Ui| {
            ui.small("Enable/disable changes runtime admission on the next launch.");
            ui.horizontal_wrapped(|ui| {
            if ui.button("Refresh inventory").clicked() { send(Request::Packages, commands); }
            if ui.button("Diagnose all").clicked() { send(Request::Diagnose(None), commands); }
            if ui.button("Update index").on_hover_text("Refresh configured indexes using the current source and signature policy; progress and failures appear in background jobs.").clicked() {
                send(Request::UpdateIndex, commands);
            }
        });
            let source_id = egui::Id::new("package-source");
            let kind_id = source_id.with("kind");
            let mut kind = ui
                .data(|data| data.get_temp::<SourceKind>(kind_id))
                .unwrap_or(SourceKind::Index);
            egui::ComboBox::from_id_salt(kind_id)
                .selected_text(kind.label())
                .show_ui(ui, |ui| {
                    for value in [
                        SourceKind::Index,
                        SourceKind::Directory,
                        SourceKind::Archive,
                        SourceKind::Legacy,
                        SourceKind::Url,
                    ] {
                        ui.selectable_value(&mut kind, value, value.label());
                    }
                });
            ui.data_mut(|data| data.insert_temp(kind_id, kind));
            let mut source = ui
                .data_mut(|data| data.remove_temp::<String>(source_id))
                .unwrap_or_default();
            ui.add(
                egui::TextEdit::singleline(&mut source)
                    .char_limit(4096)
                    .hint_text(kind.label()),
            );
            ui.horizontal(|ui| {
                if ui.button("Search index").clicked() {
                    send(Request::PackageSearch(source.clone()), commands);
                }
                if ui.button("Install…").clicked() {
                    ui.data_mut(|data| {
                        data.insert_temp(
                            egui::Id::new("package-confirm"),
                            (
                                panel.packages_revision,
                                Request::InstallFrom {
                                    source: source.clone(),
                                    kind,
                                },
                            ),
                        )
                    });
                }
            });
            ui.data_mut(|data| data.insert_temp(source_id, source));
            egui::ScrollArea::vertical()
                .id_salt("package-inventory")
                .max_height(240.0)
                .show(ui, |ui| {
                    for package in panel.packages.iter() {
                        ui.group(|ui| {
                            ui.label(format!(
                                "{} {}",
                                if package.name.is_empty() {
                                    &package.id
                                } else {
                                    &package.name
                                },
                                package.version
                            ));
                            ui.small(&package.summary);
                            ui.horizontal_wrapped(|ui| {
                                if package.kind == Some(PluginKind::Legacy) {
                                    if ui.link("Legacy .ini configuration").clicked() {
                                        send(Request::LegacyConfiguration(package.id.clone()), commands);
                                    }
                                } else if let Some(plugin) = &package.plugin {
                                    if ui.button("Configure").clicked() {
                                        send(Request::BrowsePlugin(plugin.clone()), commands);
                                    }
                                }
                                if ui
                                    .add_enabled(
                                        package.plugin.is_none() || package.kind.is_some(),
                                        egui::Button::new("Package details"),
                                    )
                                    .on_hover_text(if package.plugin.is_some() {
                                        "Inspect discovered local package metadata without executing it"
                                    } else {
                                        "Show details from the configured package indexes"
                                    })
                                    .clicked()
                                {
                                    let target = if package.plugin.is_some() {
                                        package.kind.map(|kind| PackageDetailsTarget::Local {
                                            kind,
                                            id: package.id.clone(),
                                        })
                                    } else {
                                        Some(PackageDetailsTarget::Index {
                                            id: package.id.clone(),
                                        })
                                    };
                                    if let Some(target) = target {
                                        send(Request::Details(target), commands);
                                    }
                                }
                                if let (Some(plugin), Some(enabled)) = (&package.plugin, package.enabled) {
                                    if ui.button(if enabled { "Disable" } else { "Enable" }).clicked() {
                                        send(
                                            Request::Enable {
                                                plugin: plugin.clone(),
                                                enabled: !enabled,
                                            },
                                            commands,
                                        );
                                    }
                                }
                                if ui.button("Diagnose").clicked() {
                                    send(package_diagnosis_request(package), commands);
                                }
                                if package.installed {
                                    if let Some(kind) = package.kind {
                                        if ui.button("Remove…").clicked() {
                                            ui.data_mut(|data| {
                                                data.insert_temp(
                                                    egui::Id::new("package-confirm"),
                                                    (
                                                        panel.packages_revision,
                                                        Request::Remove {
                                                            kind,
                                                            id: package.id.clone(),
                                                        },
                                                    ),
                                                )
                                            });
                                        }
                                    }
                                } else if package.plugin.is_some() {
                                    ui.small("Operator-discovered package; not a managed installation.");
                                } else if ui.button("Install…").clicked() {
                                    ui.data_mut(|data| {
                                        data.insert_temp(
                                            egui::Id::new("package-confirm"),
                                            (panel.packages_revision, Request::Install(package.id.clone())),
                                        )
                                    });
                                }
                            });
                        });
                    }
                });
            let confirm_id = egui::Id::new("package-confirm");
            if ui
                .data(|data| data.get_temp::<(u64, Request)>(confirm_id))
                .is_some_and(|(revision, _)| revision != panel.packages_revision)
            {
                ui.data_mut(|data| data.remove::<(u64, Request)>(confirm_id));
            }
            if let Some((_revision, request)) = ui
                .data(|data| data.get_temp::<(u64, Request)>(confirm_id))
                .filter(|(revision, _)| *revision == panel.packages_revision)
            {
                ui.group(|ui| {
                match &request {
                    Request::Remove { kind, id } => { ui.label(format!("Remove {} package {id}", kind.directory_name())); }
                    Request::Install(id) => { ui.label(format!("Install package {id}")); }
                    Request::InstallFrom { kind, .. } => { ui.label(format!("Install from {}", kind.label())); }
                    _ => (),
                }
                ui.label("Install/remove permanently closes every third-party runtime, command, hotkey and plugin page until the next launcher start. Built-in search, settings and activation remain available. Closing this page does not cancel the job.");
                ui.horizontal(|ui| {
                    if ui.button("Confirm operation").clicked() { send(request, commands); ui.data_mut(|data| data.remove::<(u64, Request)>(confirm_id)); }
                    if ui.button("Keep current installation").clicked() { ui.data_mut(|data| data.remove::<(u64, Request)>(confirm_id)); }
                });
            });
            }
            for job in panel.jobs.iter() {
                draw_job(ui, job, commands);
            }
        };
        if panel.category == Category::Plugins {
            ui.heading("Plugins / Background jobs");
            packages(ui);
        } else {
            egui::CollapsingHeader::new("Plugins / Background jobs")
                .id_salt("package-management")
                .show(ui, packages);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_owner_tracks_focus_not_configuration_arrival() {
        let context = egui::Context::default();
        let search = egui::Id::new("settings-search-test");
        context.memory_mut(|memory| memory.request_focus(search));
        let mut panel = Panel::default();
        let owner = focused_owner(&context, &panel);
        assert!(owner.is_some());
        panel.config = 4;
        panel.schema = 8;
        panel.loading = false;
        assert!(focused_owner(&context, &panel) == owner);
        context.memory_mut(|memory| memory.request_focus(egui::Id::new("replacement")));
        assert!(focused_owner(&context, &panel) != owner);
        context.memory_mut(|memory| memory.request_focus(search));
        panel.query_reset += 1;
        assert!(focused_owner(&context, &panel) != owner);
    }

    #[test]
    fn package_diagnosis_uses_local_identity_and_retains_index_fallback() {
        let mut package = Package {
            id: "example".into(),
            plugin: None,
            kind: None,
            name: String::new(),
            version: String::new(),
            summary: String::new(),
            installed: false,
            enabled: None,
        };
        assert_eq!(
            package_diagnosis_request(&package),
            Request::Diagnose(Some("example".into())),
        );
        for (kind, identity) in [
            (PluginKind::Modern, "modern.example"),
            (PluginKind::Native, "native.example"),
        ] {
            package.plugin = Some(PluginId(identity.into()));
            package.kind = Some(kind);
            // Discovered operator packages are local even when not managed installs.
            for installed in [false, true] {
                package.installed = installed;
                assert_eq!(
                    package_diagnosis_request(&package),
                    Request::Diagnose(Some(identity.into())),
                );
            }
        }
    }

    fn paint(
        context: &egui::Context,
        panel: &Panel,
        events: Vec<egui::Event>,
        time: f64,
    ) -> egui::FullOutput {
        paint_commands(context, panel, events, time).0
    }

    fn paint_commands(
        context: &egui::Context,
        panel: &Panel,
        events: Vec<egui::Event>,
        time: f64,
    ) -> (egui::FullOutput, Vec<UiCommand>) {
        retire_projection(context, panel, true);
        let mut commands = Vec::new();
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 720.0),
                )),
                focused: true,
                time: Some(time),
                events,
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    draw(
                        ui,
                        panel,
                        &[],
                        panel.fields.first().map(|field| field.key.as_str()),
                        &mut commands,
                    );
                });
            },
        );
        (output, commands)
    }

    #[test]
    fn conflicted_nonsecret_draft_survives_filtering_without_reauthorizing_a_stale_schema() {
        let context = egui::Context::default();
        let key = "plugins.modern.sample.settings.endpoint";
        let view = Some(ViewId(52));
        let id = editor_id(view, key, false);
        let field = Field {
            key: key.into(),
            label: "Endpoint".into(),
            description: String::new(),
            category: "Plugins".into(),
            plugin: Some(PluginId("modern.sample".into())),
            value: "initial".into(),
            source: "user-plugin".into(),
            restart: false,
            secret: false,
            toggle: None,
            value_available: true,
        };
        let mut panel = Panel {
            view,
            config: 1,
            schema: 1,
            projection_config: 1,
            projection_schema: 1,
            authoritative: true,
            fields: Arc::from([field.clone()]),
            ..Default::default()
        };
        let _ = paint(&context, &panel, Vec::new(), 0.0);
        let _ = paint(
            &context,
            &panel,
            vec![egui::Event::Text("modern-draft-retain".into())],
            1.0,
        );
        let original = context
            .data(|data| data.get_temp::<Draft>(id))
            .expect("typed draft");
        assert!(original.changed && original.text.contains("modern-draft-retain"));
        panel.config = 2;
        panel.projection_config = 2;
        let mut external = field;
        external.value = "external-writer-wins".into();
        panel.fields = Arc::from([external.clone()]);
        let _ = paint(&context, &panel, Vec::new(), 2.0);
        panel.fields = Arc::from([]);
        let _ = paint(&context, &panel, Vec::new(), 3.0);
        assert!(egui::text_edit::TextEditState::load(&context, id.with("text")).is_none());
        panel.fields = Arc::from([external.clone()]);
        let _ = paint(&context, &panel, Vec::new(), 4.0);
        let restored = context
            .data(|data| data.get_temp::<Draft>(id))
            .expect("draft restored after filtering");
        assert_eq!(restored.text, original.text);
        assert!(restored.changed && restored.config != panel.config);
        assert_eq!(restored.schema, 1);

        // A disappeared/changed declaration cannot silently rebase that draft.
        panel.schema = 2;
        panel.projection_schema = 2;
        panel.fields = Arc::from([]);
        let enter = || {
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        let (_, absent_commands) = paint_commands(&context, &panel, enter(), 5.0);
        assert!(!absent_commands
            .iter()
            .any(|command| matches!(command, UiCommand::ManageSettings(Request::Edit { .. }))));
        external.description = "Changed declaration".into();
        panel.fields = Arc::from([external]);
        let _ = paint(&context, &panel, Vec::new(), 6.0);
        context.memory_mut(|memory| memory.request_focus(id.with("text")));
        let (_, changed_commands) = paint_commands(&context, &panel, enter(), 7.0);
        assert!(!changed_commands
            .iter()
            .any(|command| matches!(command, UiCommand::ManageSettings(Request::Edit { .. }))));
        let retained = context
            .data(|data| data.get_temp::<Draft>(id))
            .expect("unrebased draft");
        assert_eq!(retained.text, original.text);
        assert_eq!(retained.schema, 1);
        assert!(retained.schema != panel.schema);
        clear_editors(&context);
        assert!(context.data(|data| data.get_temp::<Draft>(id)).is_none());
    }

    #[test]
    fn typed_schema_refusal_retires_private_state_without_wiping_nonsecret_conflicts() {
        let context = egui::Context::default();
        let view = Some(ViewId(63));
        let private_key = "plugins.native.sample.settings.api-key";
        let public_key = "plugins.native.sample.settings.endpoint";
        let private_id = editor_id(view, private_key, true);
        let public_id = editor_id(view, public_key, false);
        let secret = "schema-refusal-private-canary";
        let private_field = Field {
            key: private_key.into(),
            label: "API key".into(),
            description: String::new(),
            category: "Plugins".into(),
            plugin: Some(PluginId("native.sample".into())),
            value: String::new(),
            source: "user-plugin".into(),
            restart: false,
            secret: true,
            toggle: None,
            value_available: true,
        };
        let mut public_field = private_field.clone();
        public_field.key = public_key.into();
        public_field.label = "Endpoint".into();
        public_field.secret = false;
        public_field.value = "initial".into();
        let mut panel = Panel {
            view,
            config: 1,
            schema: 1,
            projection_config: 1,
            projection_schema: 1,
            authoritative: true,
            fields: Arc::from([private_field, public_field]),
            ..Default::default()
        };
        let _ = paint(&context, &panel, Vec::new(), 0.0);
        let _ = paint(&context, &panel, vec![egui::Event::Text(secret.into())], 1.0);
        assert!(context
            .data(|data| data.get_temp::<Draft>(private_id))
            .is_some_and(|draft| draft.text == secret));
        context.memory_mut(|memory| memory.request_focus(public_id.with("text")));
        let _ = paint(
            &context,
            &panel,
            vec![egui::Event::Text("nonsecret-retained".into())],
            2.0,
        );
        let public_text = context
            .data(|data| data.get_temp::<Draft>(public_id))
            .expect("nonsecret draft")
            .text;
        assert!(public_text.contains("nonsecret-retained"));

        // A plain configuration conflict must not revoke the private draft.
        panel.config = 2;
        let _ = paint(&context, &panel, Vec::new(), 3.0);
        assert!(context
            .data(|data| data.get_temp::<Draft>(private_id))
            .is_some_and(|draft| draft.text == secret));
        let private_focus = context
            .data(|data| data.get_temp::<egui::Id>(private_id.with("private-focus")))
            .expect("private widget identity");
        context.memory_mut(|memory| memory.request_focus(private_focus));

        // The host projects a typed StaleSchema refusal. Its retained schema
        // counter can be unchanged when the declaration source is unavailable.
        panel.schema_refusal = 92;
        panel.schema_rejected = true;
        panel.authoritative = false;
        let (output, commands) = paint_commands(&context, &panel, Vec::new(), 4.0);
        assert_eq!(panel.schema, 1);
        assert!(context.data(|data| data.get_temp::<Draft>(private_id)).is_none());
        assert!(context
            .data(|data| data.get_temp::<egui::Id>(private_id.with("private-focus")))
            .is_none());
        assert!(egui::text_edit::TextEditState::load(&context, private_id.with("text")).is_none());
        assert_ne!(context.memory(|memory| memory.focused()), Some(private_focus));
        assert!(context
            .data(|data| data.get_temp::<Editors>(egui::Id::new("managed-settings-editor-registry")))
            .is_some_and(|editors| !editors.entries.iter().any(|editor| editor.id == private_id)));
        assert!(!format!("{:?}", output.shapes).contains(secret));
        assert!(!commands
            .iter()
            .any(|command| matches!(command, UiCommand::ManageSettings(Request::Edit { .. }))));
        let retained = context
            .data(|data| data.get_temp::<Draft>(public_id))
            .expect("nonsecret conflict retained");
        assert_eq!(retained.text, public_text);
        assert!(retained.changed && retained.schema_refusal != panel.schema_refusal);

        // Freshly approved metadata may recreate an empty secret editor, never
        // the revoked buffer or its previous widget state.
        panel.authoritative = true;
        panel.schema_rejected = false;
        panel.projection_config = panel.config;
        let _ = paint(&context, &panel, Vec::new(), 5.0);
        assert!(context
            .data(|data| data.get_temp::<Draft>(private_id))
            .is_some_and(|draft| draft.text.is_empty() && !draft.changed && draft.pending.is_none()));
        context.memory_mut(|memory| memory.request_focus(public_id.with("text")));
        let (_, commands) = paint_commands(
            &context,
            &panel,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            6.0,
        );
        assert!(!commands
            .iter()
            .any(|command| matches!(command, UiCommand::ManageSettings(Request::Edit { .. }))));
        assert!(context
            .data(|data| data.get_temp::<Draft>(public_id))
            .is_some_and(|draft| draft.text == public_text && draft.schema_refusal != panel.schema_refusal));

        let private_focus = context
            .data(|data| data.get_temp::<egui::Id>(private_id.with("private-focus")))
            .expect("fresh private widget");
        context.memory_mut(|memory| memory.request_focus(private_focus));
        let _ = paint(&context, &panel, vec![egui::Event::Text(secret.into())], 7.0);
        assert!(context
            .data(|data| data.get_temp::<Draft>(private_id))
            .is_some_and(|draft| draft.text == secret));
        panel.category = Category::Appearance;
        let _ = paint(&context, &panel, Vec::new(), 8.0);
        assert!(context.data(|data| data.get_temp::<Draft>(private_id)).is_none());
        assert_ne!(context.memory(|memory| memory.focused()), Some(private_focus));
        panel.category = Category::Plugins;
        let _ = paint(&context, &panel, Vec::new(), 9.0);
        assert!(context
            .data(|data| data.get_temp::<Draft>(private_id))
            .is_some_and(|draft| draft.text.is_empty() && !draft.changed));
        assert!(context
            .data(|data| data.get_temp::<Draft>(public_id))
            .is_some_and(|draft| draft.text == public_text && draft.schema_refusal != panel.schema_refusal));

        let private_focus = context
            .data(|data| data.get_temp::<egui::Id>(private_id.with("private-focus")))
            .expect("private widget in Plugins");
        context.memory_mut(|memory| memory.request_focus(private_focus));
        let _ = paint(&context, &panel, vec![egui::Event::Text(secret.into())], 10.0);
        assert!(context
            .data(|data| data.get_temp::<Draft>(private_id))
            .is_some_and(|draft| draft.text == secret));
        // The same field remains present in All: category retirement cannot
        // depend on field visibility or wait for the asynchronous projection.
        panel.category = Category::All;
        panel.loading = true;
        retire_projection(&context, &panel, true);
        assert!(context.data(|data| data.get_temp::<Draft>(private_id)).is_none());
        assert!(context
            .data(|data| data.get_temp::<egui::Id>(private_id.with("private-focus")))
            .is_none());
        assert!(egui::text_edit::TextEditState::load(&context, private_id.with("text")).is_none());
        assert_ne!(context.memory(|memory| memory.focused()), Some(private_focus));
        assert!(context
            .data(|data| data.get_temp::<Draft>(public_id))
            .is_some_and(|draft| draft.text == public_text && draft.schema_refusal != panel.schema_refusal));
    }

    #[test]
    fn retiring_a_secret_field_cannot_replay_it_into_a_later_text_editor() {
        let context = egui::Context::default();
        let secret = "private-settings-undo-canary";
        let view = Some(ViewId(41));
        let secret_key = "plugins.native.sample.settings.password";
        let secret_id = editor_id(view, secret_key, true);
        let mut panel = Panel {
            view,
            config: 1,
            schema: 1,
            projection_config: 1,
            projection_schema: 1,
            authoritative: true,
            fields: Arc::from([Field {
                key: secret_key.into(),
                label: "Value".into(),
                description: String::new(),
                category: "Plugins".into(),
                plugin: Some(PluginId("native.sample".into())),
                value: String::new(),
                source: "user-plugin".into(),
                restart: false,
                secret: true,
                toggle: None,
                value_available: true,
            }]),
            ..Default::default()
        };
        let _ = paint(&context, &panel, Vec::new(), 0.0);
        let output = paint(&context, &panel, vec![egui::Event::Text(secret.into())], 1.0);
        assert!(context
            .data(|data| data.get_temp::<Draft>(secret_id))
            .is_some_and(|draft| draft.text == secret));
        assert!(egui::text_edit::TextEditState::load(&context, secret_id.with("text")).is_none());
        assert!(!format!("{:?}", output.shapes).contains(secret));
        assert!(!format!(
            "{:?}",
            Request::Edit {
                request_id: 1,
                key: secret_key.into(),
                plugin: None,
                draft: secret.into(),
                config: 1,
                schema: 1,
            }
        )
        .contains(secret));
        let mut debug_field = panel.fields[0].clone();
        debug_field.value = secret.into();
        assert!(!format!("{debug_field:?}").contains(secret));
        panel.schema = 2;
        panel.projection_schema = 2;
        let _ = paint(&context, &panel, Vec::new(), 1.5);
        assert!(context
            .data(|data| data.get_temp::<Draft>(secret_id))
            .is_some_and(|draft| draft.text.is_empty()));
        let mut ordinary = panel.fields[0].clone();
        ordinary.key = "plugins.native.sample.settings.label".into();
        ordinary.secret = false;
        ordinary.value = "public".into();
        panel.fields = Arc::from([ordinary]);
        let _ = paint(&context, &panel, Vec::new(), 2.0);
        assert!(context.data(|data| data.get_temp::<Draft>(secret_id)).is_none());
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        let output = paint(
            &context,
            &panel,
            vec![egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            3.0,
        );
        let ordinary_id = editor_id(view, &panel.fields[0].key, false);
        assert!(context
            .data(|data| data.get_temp::<Draft>(ordinary_id))
            .is_some_and(|draft| !draft.text.contains(secret)));
        assert!(!format!("{:?}", output.shapes).contains(secret));
        clear_editors(&context);
        assert!(egui::text_edit::TextEditState::load(&context, ordinary_id.with("text")).is_none());
        assert!(context.data(|data| data.get_temp::<Draft>(ordinary_id)).is_none());
    }

    fn deliver_acknowledgement(context: &egui::Context, draft: Draft) -> Draft {
        let view = Some(ViewId(1));
        let key = "plugins.native.sample.settings.password";
        let id = editor_id(view, key, true);
        context.data_mut(|data| data.insert_temp(id, draft));
        let panel = Panel {
            view,
            config: 2,
            schema: 1,
            projection_config: 2,
            projection_schema: 1,
            authoritative: true,
            acknowledged: Some((key.into(), 7)),
            fields: Arc::from([Field {
                key: key.into(),
                label: "Password".into(),
                description: String::new(),
                category: "Plugins".into(),
                plugin: Some(PluginId("native.sample".into())),
                value: String::new(),
                source: "user-plugin".into(),
                restart: false,
                secret: true,
                toggle: None,
                value_available: true,
            }]),
            ..Default::default()
        };
        let _ = context.run(Default::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| draw(ui, &panel, &[], None, &mut Vec::new()));
        });
        context
            .data_mut(|data| data.remove_temp::<Draft>(id))
            .expect("editor is retained")
    }

    #[test]
    fn earlier_save_does_not_discard_a_newer_private_draft() {
        let draft = deliver_acknowledgement(
            &egui::Context::default(),
            Draft {
                text: "newer-private-draft".into(),
                config: 1,
                schema: 1,
                schema_refusal: 0,
                changed: true,
                revision: 2,
                pending: Some((7, 1)),
                focused: false,
            },
        );
        assert_eq!(draft.text, "newer-private-draft");
        assert!(draft.changed);
        assert!(draft.pending.is_none());
        assert_eq!(draft.config, 1, "newer draft still requires an explicit rebase");
    }

    #[test]
    fn matching_secret_save_retires_only_the_acknowledged_private_draft() {
        let draft = deliver_acknowledgement(
            &egui::Context::default(),
            Draft {
                text: "submitted-private-draft".into(),
                config: 1,
                schema: 1,
                schema_refusal: 0,
                changed: true,
                revision: 1,
                pending: Some((7, 1)),
                focused: false,
            },
        );
        assert!(draft.text.is_empty());
        assert!(!draft.changed);
        assert!(draft.pending.is_none());
        assert_eq!(draft.config, 2);
    }
}
