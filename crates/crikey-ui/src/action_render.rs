//! Native action sheet; all matching and validation belongs to the bounded model.
use super::*;
use crate::action_panel::{ActionIdentity, ActionPanelInvalidation};

pub(super) const HEIGHT: f32 = 244.0;

pub(super) fn keyboard(context: &egui::Context, model: &ViewModel, commands: &mut Vec<UiCommand>) {
    context.data_mut(|data| data.insert_temp(egui::Id::new("crikey-navigation-activate"), false));
    if model.settings_open || model.page.is_some() || model.navigation.inline_arguments.is_some() {
        return;
    }
    use crate::navigation::NavigationAction;
    let id = egui::Id::new("crikey-action-ime");
    let activate_id = egui::Id::new("crikey-navigation-activate");
    context.data_mut(|data| data.insert_temp(activate_id, false));
    let navigation = context
        .data(|data| data.get_temp::<Arc<crate::navigation::CompiledNavigation>>(navigation_id()))
        .expect("launcher context owns its compiled navigation");
    let focused = context
        .data(|data| data.get_temp::<Option<egui::Id>>(input_focus_id()))
        .unwrap_or_else(|| context.memory(|memory| memory.focused()));
    let initial_owner = navigation_owner_for_focus(context, Some(model), focused);
    // egui folds arrow/Enter focus traversal before widget paint. Preserve the
    // actual batch-start search owner while its canonical navigation is handled.
    if initial_owner == crate::navigation::NavigationOwner::Query {
        if let Some(id) = focused {
            context.memory_mut(|memory| memory.request_focus(id));
        }
    }
    let query_focused = focused == Some(query_edit_id());
    let collection_id = query_edit_id().with("collection-navigation");
    let collection_scope = is_standard_collection(model) && model.action_panel.is_none();
    let mut collection = is_standard_collection(model)
        && context.data(|data| data.get_temp::<bool>(collection_id).unwrap_or(true));
    let mut composing = context.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let mut submitted = false;
    context.input_mut(|input| {
        let altgr_text = input.events.iter().any(|event| matches!(event, egui::Event::Text(text) if text.chars().any(|character| !character.is_control())));
        input.events.retain_mut(|event| {
            if collection_scope && !submitted && matches!(event, egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Ime(_)) {
                collection = false;
            }
            match event {
                egui::Event::Ime(egui::ImeEvent::Enabled) => composing = true,
                egui::Event::Ime(egui::ImeEvent::Preedit(text)) => composing |= !text.is_empty(),
                egui::Event::Ime(egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled) => composing = false,
                egui::Event::Key { key, modifiers, pressed: true, .. } if !submitted => {
                    if modifiers.ctrl && modifiers.alt && altgr_text { return true; }
                    let key_name = match *key {
                        egui::Key::ArrowUp => "Up",
                        egui::Key::ArrowDown => "Down",
                        egui::Key::ArrowLeft => "Left",
                        egui::Key::ArrowRight => "Right",
                        other => other.name(),
                    };
                    let platform_modifiers = crikey_platform::Modifiers {
                        ctrl: modifiers.ctrl || (modifiers.command && !modifiers.mac_cmd),
                        alt: modifiers.alt, shift: modifiers.shift, meta: modifiers.mac_cmd,
                    };
                    let chord = navigation_chord(&navigation, key_name, platform_modifiers);
                    let owner = if collection_scope && query_focused && matches!(initial_owner, crate::navigation::NavigationOwner::Host | crate::navigation::NavigationOwner::Query) {
                        if collection { crate::navigation::NavigationOwner::Host } else { crate::navigation::NavigationOwner::Query }
                    } else { initial_owner };
                    let route = navigation_context(model, owner, false);
                    let action = chord.and_then(|chord| navigation.resolve(chord, route));
                    if composing && (action.is_some() || *key == egui::Key::Enter) {
                        return false;
                    }
                    if let Some(action) = action {
                        if action == NavigationAction::Activate {
                            // Let the focused single-line editor admit the text prefix
                            // and stop at this key, including a remapped activation.
                            *key = egui::Key::Enter;
                            *modifiers = egui::Modifiers::NONE;
                            submitted = true;
                        } else if let Some(command) = navigation_command(action, model) {
                            if collection_scope && matches!(action, NavigationAction::Next | NavigationAction::Previous | NavigationAction::PageDown | NavigationAction::PageUp | NavigationAction::Left | NavigationAction::Right) {
                                collection = true;
                            }
                            commands.push(command);
                            return false;
                        }
                    }
                }
                _ => {}
            }
            true
        });
    });
    context.data_mut(|data| {
        data.insert_temp(id, composing);
        data.insert_temp(activate_id, submitted);
        if collection_scope {
            data.insert_temp(collection_id, collection);
        }
    });
}
pub(super) fn draw(
    ui: &mut egui::Ui,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    colors: &theme::Palette,
) {
    let Some(panel) = &model.action_panel else {
        return;
    };
    let target = panel.tree().target();
    let row = model.rows.iter().find(|row| target.matches(row));
    Frame::default()
        .fill(colors.surface)
        .rounding(Rounding::same(colors.metrics.radius_medium))
        .inner_margin(Margin::same(colors.metrics.space_3))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_height(colors.metrics.scaled(HEIGHT) - 2.0 * colors.metrics.space_3);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Actions").strong());
                if ui.small_button("Back").clicked() {
                    commands.push(UiCommand::ActionBack);
                }
                draw_navigation_hint(ui, model, crate::navigation::NavigationAction::Cancel);
                for identity in panel.path() {
                    if let Some(presentation) = panel.tree().presentation(identity) {
                        ui.add(egui::Label::new(&presentation.label).truncate());
                    }
                }
            });
            if let Some(reason) = panel.invalidation() {
                ui.colored_label(
                    colors.warning,
                    match reason {
                        ActionPanelInvalidation::TargetUnavailable => {
                            "This result is no longer available. Close Actions and select a current result."
                        }
                        ActionPanelInvalidation::ActionUnavailable => {
                            "This action is no longer available. Close Actions to choose again."
                        }
                    },
                );
                return;
            }
            let mut query = panel.query().to_owned();
            let focus_id = egui::Id::new("crikey-action-initial-focus");
            let initial_focus =
                ui.ctx().data(|data| data.get_temp::<u64>(focus_id)) != Some(panel.focus_token());
            let argument_editor = row.is_some_and(|row| {
                row.command_state.as_ref().is_some_and(|state| {
                    state.argument_policy != crikey_core::ArgumentPolicy::Forbidden
                        && state.argument.is_none()
                })
            });
            if initial_focus && !argument_editor {
                ui.memory_mut(|memory| memory.request_focus(action_search_id()));
            }
            let response = ui.add_sized(
                [ui.available_width(), colors.metrics.control_height],
                TextEdit::singleline(&mut query)
                    .id(action_search_id())
                    .hint_text("Search actions")
                    .char_limit(crate::action_panel::MAX_ACTION_QUERY_BYTES)
                    .lock_focus(true),
            );

            ui.ctx()
                .data_mut(|data| data.insert_temp(focus_id, panel.focus_token()));
            if response.changed() {
                commands.push(UiCommand::SetActionQuery(query));
            }
            // Single-line TextEdit stops at Return, so this query includes text
            // before the key but never text that arrived after submission.
            if ui.ctx().data(|data| {
                data.get_temp::<bool>(egui::Id::new("crikey-navigation-activate"))
                    .unwrap_or(false)
            }) {
                commands.push(UiCommand::ActivateSelectedAction);
            }
            let scroll_id = egui::Id::new("crikey-action-scroll-selection");
            let previous = ui
                .ctx()
                .data(|data| data.get_temp::<(u64, Option<ActionIdentity>)>(scroll_id));
            let selection_changed = previous.as_ref().is_some_and(|(token, selected)| {
                *token == panel.focus_token() && selected.as_ref() != panel.selected()
            });
            if previous.as_ref().is_none_or(|(token, selected)| {
                *token != panel.focus_token() || selected.as_ref() != panel.selected()
            }) {
                ui.ctx().data_mut(|data| {
                    data.insert_temp(scroll_id, (panel.focus_token(), panel.selected().cloned()))
                });
            }
            egui::ScrollArea::vertical()
                .id_salt("crikey-action-entries")
                .max_height(ui.available_height())
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Keep the prepared command configuration action reachable
                    // above the inline preference controls in the short sheet.
                    let configure = panel.visible_entries().find(|entry| {
                        entry.identity
                            == &ActionIdentity::Host(crate::action_panel::HostAction::ConfigureCommand)
                    });
                    let mut any = configure.is_some();
                    if let Some(entry) = configure {
                        let response = ui
                            .selectable_label(entry.selected, &entry.presentation.label)
                            .on_hover_text(&entry.presentation.description);
                        if response.clicked() {
                            commands.push(UiCommand::ActivateAction {
                                target: target.clone(),
                                identity: entry.identity.clone(),
                            });
                        }
                        if selection_changed && entry.selected && !response.rect.intersects(ui.clip_rect()) {
                            response.scroll_to_me(None);
                        }
                    }
                    if let Some(row) = row {
                        draw_command_controls(
                            ui,
                            row,
                            model,
                            commands,
                            initial_focus && argument_editor,
                            colors,
                        );
                    }
                    for command in panel.tree().management_rows() {
                        let crate::ActionTarget::Command(reference) = &command.target else {
                            continue;
                        };
                        egui::CollapsingHeader::new(format!(
                            "{} — {} / {}",
                            command.label, reference.provider_id.0, reference.command_id
                        ))
                        .id_salt(("command-management", &command.target))
                        .show(ui, |ui| {
                            draw_command_controls(ui, command, model, commands, false, colors)
                        });
                    }
                    let mut section = "";
                    let icon_size = colors.metrics.scaled(16.0);
                    let row_height = (colors.metrics.control_height
                        + 2.0 * (colors.metrics.row_pad_y - theme::ROW_PAD_Y * colors.metrics.scale))
                        .max(ui.text_style_height(&TextStyle::Button));
                    for entry in panel.visible_entries() {
                        if entry.identity
                            == &ActionIdentity::Host(crate::action_panel::HostAction::ConfigureCommand)
                        {
                            continue;
                        }
                        any = true;
                        if !panel.is_searching() && section != entry.section_id {
                            section = entry.section_id;
                            ui.label(
                                RichText::new(entry.section_label)
                                    .small()
                                    .color(colors.text_muted),
                            );
                        }
                        let presentation = entry.presentation;
                        let color = if presentation.destructive {
                            colors.error
                        } else {
                            colors.text
                        };
                        let response = ui
                            .horizontal(|ui| {
                                // Only already-prepared pixels may be used. Unresolved references
                                // get a host vector symbol, never a synchronous resource load.
                                let icon = row
                                    .filter(|row| row.icon_reference == presentation.icon_reference)
                                    .and_then(|row| row.icon.as_deref());
                                if let Some(icon) = icon {
                                    let texture = icon_texture(ui.ctx(), icon);
                                    ui.image((texture.id(), vec2(icon_size, icon_size)));
                                } else {
                                    let (rect, _) = ui.allocate_exact_size(
                                        vec2(icon_size, icon_size),
                                        egui::Sense::hover(),
                                    );
                                    let painter = ui.painter();
                                    if matches!(
                                        entry.identity,
                                        ActionIdentity::Host(_) | ActionIdentity::Submenu(_)
                                    ) {
                                        painter.circle_stroke(
                                            rect.center(),
                                            6.0,
                                            Stroke::new(1.4, colors.text_muted),
                                        );
                                        painter.circle_stroke(
                                            rect.center(),
                                            2.0,
                                            Stroke::new(1.4, colors.text_muted),
                                        );
                                    } else {
                                        painter.line_segment(
                                            [rect.left_center(), rect.right_center()],
                                            Stroke::new(1.4, color),
                                        );
                                        painter.line_segment(
                                            [rect.right_center(), rect.center_top()],
                                            Stroke::new(1.4, color),
                                        );
                                        painter.line_segment(
                                            [rect.right_center(), rect.center_bottom()],
                                            Stroke::new(1.4, color),
                                        );
                                    }
                                }
                                let available = ui.available_width().max(0.0);
                                let label_width = available * 0.68;
                                let text =
                                    egui::WidgetText::from(RichText::new(&presentation.label).color(color))
                                        .into_galley(
                                            ui,
                                            Some(egui::TextWrapMode::Truncate),
                                            (label_width - 2.0 * ui.spacing().button_padding.x).max(0.0),
                                            TextStyle::Button,
                                        );
                                let response = ui
                                    .add_sized(
                                        [label_width, row_height],
                                        egui::SelectableLabel::new(entry.selected, text),
                                    )
                                    .on_hover_ui(|ui| {
                                        ui.label(&presentation.label);
                                        if !presentation.description.is_empty() {
                                            ui.label(&presentation.description);
                                        }
                                    });
                                if entry.submenu {
                                    ui.label(">");
                                }
                                if let Some(shortcut) = &presentation.shortcut {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(shortcut).small().color(colors.text_muted),
                                        )
                                        .truncate(),
                                    );
                                }
                                if presentation.destructive {
                                    ui.add(
                                        egui::Label::new(RichText::new("Destructive").small().color(color))
                                            .truncate(),
                                    );
                                }
                                if entry.selected
                                    && !entry.submenu
                                    && ui.available_width() >= colors.metrics.scaled(20.0)
                                {
                                    draw_navigation_hint(
                                        ui,
                                        model,
                                        crate::navigation::NavigationAction::Activate,
                                    );
                                }
                                if panel.is_searching() {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(entry.breadcrumb).small().color(colors.text_muted),
                                        )
                                        .truncate(),
                                    );
                                }
                                response
                            })
                            .inner;
                        if response.clicked() {
                            if entry.identity == &ActionIdentity::InvokeCommand {
                                if let Some(command) =
                                    row.and_then(|row| row.primary_command(model.generation, &model.query))
                                {
                                    commands.push(command);
                                }
                            } else {
                                commands.push(UiCommand::ActivateAction {
                                    target: target.clone(),
                                    identity: entry.identity.clone(),
                                });
                            }
                        }
                        if selection_changed && entry.selected && !response.rect.intersects(ui.clip_rect()) {
                            response.scroll_to_me(None);
                        }
                    }
                    if !any {
                        ui.label("No matching actions");
                    }
                });
        });
}

pub(super) fn draw_command_controls(
    ui: &mut egui::Ui,
    row: &ResultRow,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    focus_argument: bool,
    colors: &theme::Palette,
) {
    let (crate::ActionTarget::Command(reference), Some(state)) = (&row.target, &row.command_state) else {
        return;
    };
    ui.label(RichText::new(&row.label).strong());
    ui.label(RichText::new(format!("{} / {}", reference.provider_id.0, reference.command_id)).small());
    if let Some(reason) = &state.unavailable_reason {
        ui.label(reason);
    }
    if let Some(status) = model
        .command_management
        .iter()
        .find(|managed| managed.target == row.target)
        .and_then(|managed| managed.status.as_ref())
        .or(row.status.as_ref())
    {
        ui.label(status);
    }
    if !model.settings_open
        && state.argument_policy != crikey_core::ArgumentPolicy::Forbidden
        && state.argument.is_none()
    {
        let id = egui::Id::new((
            "command-argument",
            &row.target,
            state.registry_revision,
            model.generation.get(),
        ));
        let mut argument = ui
            .ctx()
            .data_mut(|data| data.get_temp::<String>(id).unwrap_or_default());
        let response = ui.add(TextEdit::singleline(&mut argument).id(id).hint_text(
            if state.argument_policy == crikey_core::ArgumentPolicy::Required {
                "Required argument"
            } else {
                "Optional argument"
            },
        ));
        let focus_id = id.with("focused");
        if focus_argument
            || !ui
                .ctx()
                .data_mut(|data| data.get_temp::<bool>(focus_id).unwrap_or(false))
        {
            response.request_focus();
            ui.ctx().data_mut(|data| data.insert_temp(focus_id, true));
        }
        ui.ctx().data_mut(|data| data.insert_temp(id, argument.clone()));
        let submitted = response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if ui
            .add_enabled(state.available, egui::Button::new("Invoke with argument"))
            .clicked()
            || submitted
        {
            commands.push(UiCommand::InvokeCommand {
                target: row.target.clone(),
                generation: model.generation,
                query: model.query.clone(),
                registry_revision: state.registry_revision,
                argument: (!argument.is_empty()).then_some(argument),
            });
        }
    }
    for (hotkey, label, value) in [
        (
            false,
            "Command alias (separate from textual aliases)",
            state.aliases.first().cloned().unwrap_or_default(),
        ),
        (
            true,
            "Global command shortcut",
            state.hotkey.clone().unwrap_or_default(),
        ),
    ] {
        let key = crate::command_edit_key(reference, hotkey);
        let row = SettingRow {
            key: key.clone(),
            label: label.into(),
            value,
            source: "effective command preferences".into(),
            control: SettingControl::Text,
        };
        let start = commands.len();
        draw_setting_row(
            ui,
            &row,
            &model.setting_edits,
            model.settings_focus.as_deref(),
            commands,
            colors,
        );
        for command in &mut commands[start..] {
            if let UiCommand::SetSetting {
                request_id, value, ..
            } = command
            {
                *command = UiCommand::EditCommandShortcut {
                    request_id: *request_id,
                    target: crate::ActionTarget::Command(reference.clone()),
                    expected_revision: state.preference_revision,
                    edit: if hotkey {
                        crate::CommandPreferenceEdit::SetHotkey((!value.is_empty()).then(|| value.clone()))
                    } else {
                        crate::CommandPreferenceEdit::SetAlias((!value.is_empty()).then(|| value.clone()))
                    },
                };
            }
        }
        if hotkey {
            if model.settings_focus.as_deref() == Some(format!("record:{key}").as_str()) {
                ui.label("Press a shortcut; Escape cancels");
                if ui.button("Cancel recording").clicked() {
                    commands.push(UiCommand::CancelShortcutCapture);
                }
            } else if ui.button("Record shortcut").clicked() {
                commands.push(UiCommand::BeginShortcutCapture(key));
            }
        }
    }
    let mut edits = Vec::new();
    ui.horizontal_wrapped(|ui| {
        let mut favorite = state.favorite.is_some();
        if ui.checkbox(&mut favorite, "Favorite / Home").changed() {
            edits.push(crate::CommandPreferenceEdit::Favorite(favorite));
        }
        let mut hidden = state.hidden;
        if ui.checkbox(&mut hidden, "Hidden from discovery").changed() {
            edits.push(crate::CommandPreferenceEdit::Hidden(hidden));
        }
        let mut fallback = state.fallback.is_some();
        let compatible = state.argument_policy != crikey_core::ArgumentPolicy::Forbidden;
        if ui
            .add_enabled(
                fallback || (state.available && compatible),
                egui::Checkbox::new(&mut fallback, "Fallback"),
            )
            .changed()
        {
            edits.push(crate::CommandPreferenceEdit::Fallback(fallback));
        }
        if !compatible {
            ui.label("Does not accept fallback arguments");
        }
    });
    for (label, position, count, favorite) in [
        ("Favorite", state.favorite, state.favorite_count, true),
        ("Fallback", state.fallback, state.fallback_count, false),
    ] {
        if let Some(position) = position {
            ui.horizontal(|ui| {
                ui.label(format!("{label} position {}", position + 1));
                for (caption, enabled, destination) in [
                    ("Move up", position > 0, position.saturating_sub(1)),
                    ("Move down", position + 1 < count, position.saturating_add(1)),
                ] {
                    if ui.add_enabled(enabled, egui::Button::new(caption)).clicked() {
                        edits.push(if favorite {
                            crate::CommandPreferenceEdit::MoveFavorite {
                                position: destination,
                            }
                        } else {
                            crate::CommandPreferenceEdit::MoveFallback {
                                position: destination,
                            }
                        });
                    }
                }
            });
        }
    }
    for edit in edits {
        commands.push(UiCommand::EditCommandPreference {
            target: row.target.clone(),
            expected_revision: state.preference_revision,
            edit,
        });
    }
    if ui
        .add_enabled(
            state.ranking_reset_available,
            egui::Button::new("Reset learned item ranking (shared by commands targeting this item)"),
        )
        .on_disabled_hover_text("No current verified item target; host-only commands have no item ranking")
        .clicked()
    {
        commands.push(UiCommand::ResetCommandRanking {
            target: row.target.clone(),
            registry_revision: state.registry_revision,
        });
    }
    ui.separator();
}
