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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Panel {
    pub view: Option<ViewId>,
    pub request: u64,
    pub config: u64,
    pub schema: u64,
    pub projection_config: u64,
    pub projection_schema: u64,
    pub authoritative: bool,
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
    changed: bool,
    revision: u64,
    pending: Option<(u64, u64)>,
    focused: bool,
}

#[derive(Clone, Default)]
struct Editors {
    view: Option<ViewId>,
    ids: Vec<egui::Id>,
}

fn editor_id(view: Option<ViewId>, key: &str, secret: bool) -> egui::Id {
    egui::Id::new(("managed-setting", view.map(ViewId::get), key, secret))
}
fn retire_text_state(context: &egui::Context, id: egui::Id) {
    context.data_mut(|data| data.remove::<egui::text_edit::TextEditState>(id.with("text")));
    context.memory_mut(|memory| memory.surrender_focus(id.with("text")));
}
fn retire_projection(context: &egui::Context, panel: &Panel, open: bool) {
    let registry = egui::Id::new("managed-settings-editor-registry");
    let mut editors = context
        .data_mut(|data| data.remove_temp::<Editors>(registry))
        .unwrap_or_default();
    let same_view = open && editors.view == panel.view;
    editors.ids.retain(|id| {
        if same_view
            && panel
                .fields
                .iter()
                .any(|field| editor_id(panel.view, &field.key, field.secret) == *id)
        {
            return true;
        }
        context.data_mut(|data| data.remove::<Draft>(*id));
        retire_text_state(context, *id);
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
    retire_text_state(ui.ctx(), id);
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
    egui::ScrollArea::vertical().id_salt("settings-fields").max_height(320.0).show(ui, |ui| {
        for field in panel.fields.iter() {
            let id = editor_id(panel.view, &field.key, field.secret);
            let registry = egui::Id::new("managed-settings-editor-registry");
            let admitted = ui.data_mut(|data| {
                let mut editors = data.remove_temp::<Editors>(registry).unwrap_or_default();
                if !editors.ids.contains(&id) {
                    if editors.ids.len() == 128 {
                        let Some(clean) = editors.ids.iter().position(|id| {
                            let Some(draft) = data.remove_temp::<Draft>(*id) else { return true; };
                            let dirty = draft.changed || draft.pending.is_some();
                            data.insert_temp(*id, draft);
                            !dirty
                        }) else { data.insert_temp(registry, editors); return false; };
                        let retired = editors.ids.remove(clean);
                        data.remove::<Draft>(retired);
                        data.remove::<egui::text_edit::TextEditState>(retired.with("text"));
                    }
                    editors.ids.push(id);
                }
                data.insert_temp(registry, editors);
                true
            });
            if !admitted {
                ui.label(format!("{} — editor capacity reached; finish/discard a draft or close Settings", field.label));
                continue;
            }
            let mut draft = ui.data_mut(|data| data.remove_temp::<Draft>(id)).unwrap_or_else(|| Draft { text: if field.secret { String::new() } else { field.value.clone() }, config: panel.config, schema: panel.schema, ..Default::default() });
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
            if !draft.changed && (draft.config != panel.config || draft.schema != panel.schema) { draft.config = panel.config; draft.schema = panel.schema; if !field.secret { draft.text.clone_from(&field.value); } }
            ui.group(|ui| {
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
                let (response, changed, commit) = draw_input(ui, id, field.secret, &mut draft.text);
                if focus == Some(field.key.as_str()) && !draft.focused {
                    response.request_focus();
                    draft.focused = true;
                }
                if changed {
                    draft.changed = true;
                    draft.revision = draft.revision.checked_add(1).expect("settings draft revision exhausted");
                }
                let conflict = draft.changed && (draft.config != panel.config || draft.schema != panel.schema);
                if conflict { ui.label("Configuration changed. Your draft is retained; explicitly rebase before saving."); }
                ui.horizontal(|ui| {
                    let can_save = draft.changed && !conflict && !panel.loading && projection_ready;
                    if (ui.add_enabled(can_save, egui::Button::new("Save")).clicked() || commit) && can_save {
                        retire_text_state(ui.ctx(), id);
                        let request_id = crate::next_setting_edit_id();
                        draft.pending = Some((request_id, draft.revision));
                        send(Request::Edit { request_id, key: field.key.clone(), plugin: field.plugin.clone(), draft: draft.text.clone(), config: draft.config, schema: draft.schema }, commands);
                    }
                    if conflict && ui.add_enabled(projection_ready, egui::Button::new("Rebase draft")).clicked() { draft.config = panel.config; draft.schema = panel.schema; }
                    if draft.changed && ui.button("Discard draft").clicked() {
                        retire_text_state(ui.ctx(), id);
                        draft = Draft { text: if field.secret { String::new() } else { field.value.clone() }, config: panel.config, schema: panel.schema, ..Default::default() };
                    }
                });
            });
            ui.data_mut(|data| data.insert_temp(id, draft));
        }
    });
    egui::CollapsingHeader::new("Plugins / Background jobs").id_salt("package-management").show(ui, |ui| {
        ui.small("Enable/disable changes runtime admission on the next launch.");
        ui.horizontal(|ui| {
            if ui.button("Refresh inventory").clicked() { send(Request::Packages, commands); }
            if ui.button("Diagnose all").clicked() { send(Request::Diagnose(None), commands); }
        });
        let source_id = egui::Id::new("package-source");
        let kind_id = source_id.with("kind");
        let mut kind = ui.data(|data| data.get_temp::<SourceKind>(kind_id)).unwrap_or(SourceKind::Index);
        egui::ComboBox::from_id_salt(kind_id).selected_text(kind.label()).show_ui(ui, |ui| {
            for value in [SourceKind::Index, SourceKind::Directory, SourceKind::Archive, SourceKind::Legacy, SourceKind::Url] {
                ui.selectable_value(&mut kind, value, value.label());
            }
        });
        ui.data_mut(|data| data.insert_temp(kind_id, kind));
        let mut source = ui.data_mut(|data| data.remove_temp::<String>(source_id)).unwrap_or_default();
        ui.add(egui::TextEdit::singleline(&mut source).char_limit(4096).hint_text(kind.label()));
        ui.horizontal(|ui| {
            if ui.button("Search index").clicked() { send(Request::PackageSearch(source.clone()), commands); }
            if ui.button("Install…").clicked() { ui.data_mut(|data| data.insert_temp(egui::Id::new("package-confirm"), (panel.packages_revision, Request::InstallFrom { source: source.clone(), kind }))); }
        });
        ui.data_mut(|data| data.insert_temp(source_id, source));
        egui::ScrollArea::vertical().id_salt("package-inventory").max_height(240.0).show(ui, |ui| {
            for package in panel.packages.iter() {
                ui.group(|ui| {
                    ui.label(format!("{} {}", if package.name.is_empty() { &package.id } else { &package.name }, package.version));
                    ui.small(&package.summary);
                    ui.horizontal(|ui| {
                        if package.kind == Some(PluginKind::Legacy) {
                            if ui.link("Legacy .ini configuration").clicked() { send(Request::LegacyConfiguration(package.id.clone()), commands); }
                        } else if let Some(plugin) = &package.plugin { if ui.button("Configure").clicked() { send(Request::BrowsePlugin(plugin.clone()), commands); } }
                        if let (Some(plugin), Some(enabled)) = (&package.plugin, package.enabled) {
                            if ui.button(if enabled { "Disable" } else { "Enable" }).clicked() { send(Request::Enable { plugin: plugin.clone(), enabled: !enabled }, commands); }
                        }
                        if ui.button("Diagnose").clicked() { send(Request::Diagnose(Some(package.id.clone())), commands); }
                        if package.installed {
                            if let Some(kind) = package.kind { if ui.button("Remove…").clicked() { ui.data_mut(|data| data.insert_temp(egui::Id::new("package-confirm"), (panel.packages_revision, Request::Remove { kind, id: package.id.clone() }))); } }
                        } else if package.plugin.is_some() {
                            ui.small("Operator-discovered package; not a managed installation.");
                        } else if ui.button("Install…").clicked() { ui.data_mut(|data| data.insert_temp(egui::Id::new("package-confirm"), (panel.packages_revision, Request::Install(package.id.clone())))); }
                    });
                });
            }
        });
        let confirm_id = egui::Id::new("package-confirm");
        if ui.data(|data| data.get_temp::<(u64, Request)>(confirm_id)).is_some_and(|(revision, _)| revision != panel.packages_revision) {
            ui.data_mut(|data| data.remove::<(u64, Request)>(confirm_id));
        }
        if let Some((_revision, request)) = ui.data(|data| data.get_temp::<(u64, Request)>(confirm_id)).filter(|(revision, _)| *revision == panel.packages_revision) {
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
            ui.horizontal(|ui| {
                ui.label(format!("{}: {}", job.operation, job.status));
                if !job.terminal && ui.button("Request cancellation").clicked() { send(Request::Cancel(job.id), commands); }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn paint(
        context: &egui::Context,
        panel: &Panel,
        events: Vec<egui::Event>,
        time: f64,
    ) -> egui::FullOutput {
        retire_projection(context, panel, true);
        context.run(
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
                        &mut Vec::new(),
                    );
                });
            },
        )
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
