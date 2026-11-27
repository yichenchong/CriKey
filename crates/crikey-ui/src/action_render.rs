//! Native action sheet; all matching and validation belongs to the bounded model.
use super::*;
use crate::action_panel::{ActionIdentity, ActionPanelInvalidation};

pub(super) const HEIGHT: f32 = 244.0;

pub(super) fn keyboard(context: &egui::Context, model: &ViewModel, commands: &mut Vec<UiCommand>) {
    if model.settings_open || model.page.is_some() {
        return;
    }
    let id = egui::Id::new("crikey-action-ime");
    let mut composing = context.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let mut submitted = false;
    context.input_mut(|input| {
        input.events.retain(|event| {
            match event {
                egui::Event::Ime(egui::ImeEvent::Enabled) => composing = true,
                egui::Event::Ime(egui::ImeEvent::Preedit(text)) => composing |= !text.is_empty(),
                egui::Event::Ime(egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled) => composing = false,
                egui::Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    ..
                } if !submitted => {
                    let plain = modifiers.matches_logically(egui::Modifiers::NONE);
                    let discovery = (*key == egui::Key::K
                        && modifiers.matches_logically(egui::Modifiers::COMMAND))
                        || (*key == egui::Key::Enter && modifiers.matches_logically(egui::Modifiers::ALT));
                    let command = if discovery {
                        Some(UiCommand::ShowActions)
                    } else if (model.action_panel.is_some() || !model.navigation.recall.is_empty()) && plain {
                        match key {
                            egui::Key::ArrowDown => Some(UiCommand::SelectNext),
                            egui::Key::ArrowUp => Some(UiCommand::SelectPrevious),
                            egui::Key::PageDown => Some(UiCommand::PageDown),
                            egui::Key::PageUp => Some(UiCommand::PageUp),
                            egui::Key::Escape => Some(if model.action_panel.is_some() {
                                UiCommand::ActionBack
                            } else {
                                UiCommand::Cancel
                            }),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    // A commit-owned key must not reach TextEdit's return path,
                    // even when a later Commit ends composition in this batch.
                    if composing && (command.is_some() || (*key == egui::Key::Enter && plain)) {
                        return false;
                    }
                    if (model.action_panel.is_some() || !model.navigation.recall.is_empty())
                        && *key == egui::Key::Enter
                        && plain
                    {
                        // The focused editor processes the prefix in event order
                        // and stops at Return. Do not move trailing host keys
                        // ahead of that submission either.
                        submitted = true;
                    } else if let Some(command) = command {
                        commands.push(command);
                        return false;
                    }
                }
                _ => {}
            }
            true
        });
    });
    context.data_mut(|data| data.insert_temp(id, composing));
}
pub(super) fn draw(
    ui: &mut egui::Ui,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    colors: theme::Palette,
) {
    let Some(panel) = &model.action_panel else {
        return;
    };
    let target = panel.tree().target();
    let row = model.rows.iter().find(|row| target.matches(row));
    Frame::default()
        .fill(colors.surface)
        .rounding(Rounding::same(theme::RADIUS_MEDIUM))
        .inner_margin(Margin::same(theme::SPACE_3))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_height(HEIGHT - 2.0 * theme::SPACE_3);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Actions").strong());
                if ui.small_button("Back / Esc").clicked() {
                    commands.push(UiCommand::ActionBack);
                }
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
            let response = ui.add_sized(
                [ui.available_width(), theme::CONTROL_HEIGHT],
                TextEdit::singleline(&mut query)
                    .id(egui::Id::new("crikey-action-search"))
                    .hint_text("Search actions")
                    .char_limit(crate::action_panel::MAX_ACTION_QUERY_BYTES)
                    .lock_focus(true),
            );
            let submitted = response.lost_focus();
            if initial_focus && !argument_editor {
                response.request_focus();
            }
            ui.ctx()
                .data_mut(|data| data.insert_temp(focus_id, panel.focus_token()));
            if response.changed() {
                commands.push(UiCommand::SetActionQuery(query));
            }
            // Single-line TextEdit stops at Return, so this query includes text
            // before the key but never text that arrived after submission.
            if submitted
                && ui.input(|input| {
                    input.events.iter().any(|event| {
                        matches!(event, egui::Event::Key {
                        key: egui::Key::Enter, pressed: true, modifiers, ..
                    } if modifiers.matches_logically(egui::Modifiers::NONE))
                    })
                })
            {
                commands.push(UiCommand::ActivateSelectedAction);
                // Return relinquishes TextEdit focus even when the host keeps
                // this panel open (for example after refusing an input batch).
                // Retain this editor, without reclaiming focus on later paints
                // from argument editors or another surface.
                response.request_focus();
            }
            // Follow selection/layout changes, not repaints: wheel and scrollbar
            // gestures must be free to leave the selected entry off screen.
            let (selection_changed, new_panel) = ui.ctx().data_mut(|data| {
                let previous = data.get_temp_mut_or_default::<(u64, u64, Option<ActionIdentity>)>(
                    egui::Id::new("crikey-action-scroll-selection"),
                );
                // Opening establishes a baseline, not a navigation intent.
                // Management controls may intentionally precede the selection.
                let same_panel = previous.0 == panel.focus_token();
                let changed =
                    previous.1 != panel.layout_revision() || previous.2.as_ref() != panel.selected();
                if !same_panel || changed {
                    *previous = (
                        panel.focus_token(),
                        panel.layout_revision(),
                        panel.selected().cloned(),
                    );
                }
                (same_panel && changed, !same_panel)
            });
            let scroll_salt = "crikey-action-entries";
            if new_panel {
                // Retire both the old offset and any in-flight scroll animation.
                egui::scroll_area::State::default()
                    .store(ui.ctx(), ui.make_persistent_id(egui::Id::new(scroll_salt)));
            }
            egui::ScrollArea::vertical()
                .id_salt(scroll_salt)
                .max_height(ui.available_height())
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(row) = row {
                        draw_command_controls(ui, row, model, commands, initial_focus && argument_editor);
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
                            draw_command_controls(ui, command, model, commands, false)
                        });
                    }
                    let mut section = "";
                    let mut any = false;
                    for entry in panel.visible_entries() {
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
                            egui::Color32::from_rgb(230, 90, 90)
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
                                    ui.image((texture.id(), vec2(16.0, 16.0)));
                                } else {
                                    let (rect, _) =
                                        ui.allocate_exact_size(vec2(16.0, 16.0), egui::Sense::hover());
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
                                let response = ui
                                    .selectable_label(
                                        entry.selected,
                                        RichText::new(&presentation.label).color(color),
                                    )
                                    .on_hover_text(&presentation.description);
                                if entry.submenu {
                                    ui.label(">");
                                }
                                if let Some(shortcut) = &presentation.shortcut {
                                    ui.label(RichText::new(shortcut).small().color(colors.text_muted));
                                }
                                if presentation.destructive {
                                    ui.label(RichText::new("Destructive").small().color(color));
                                }
                                if panel.is_searching() {
                                    ui.label(
                                        RichText::new(entry.breadcrumb).small().color(colors.text_muted),
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
                        if selection_changed && entry.selected && !ui.clip_rect().contains_rect(response.rect)
                        {
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
) {
    let (crate::ActionTarget::Command(reference), Some(state)) = (&row.target, &row.command_state) else {
        return;
    };
    ui.label(RichText::new(&row.label).strong());
    ui.label(RichText::new(format!("{} / {}", reference.provider_id.0, reference.command_id)).small());
    if let Some(reason) = &state.unavailable_reason {
        ui.label(reason);
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
