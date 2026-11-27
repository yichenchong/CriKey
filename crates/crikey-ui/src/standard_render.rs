//! Standard routes reuse native rows, prepared preview and raster ownership.
use super::*;
use crate::ActionTarget;
use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
use crikey_core::structured_view::{ActionMenuEntry, StandardView};

pub(super) fn primary_command(model: &ViewModel) -> Option<UiCommand> {
    let surface = model.navigation.standard.as_ref()?;
    if model.navigation.standard_refresh_pending {
        return None;
    }
    let executable = if let StandardView::Form(form) = &surface.descriptor.view {
        form.submit.clone()
    } else if let Some(row) = model.rows.get(model.selected) {
        let ActionTarget::Result { owner, item } = &row.target else {
            return None;
        };
        ExecutableKey {
            target: OwnedItem {
                owner: owner.clone(),
                item: item.clone(),
            },
            action: row.default_action.as_ref().map(|action| action.action_id.clone()),
        }
    } else {
        match &surface.descriptor.view {
            StandardView::Detail(detail) => {
                let target = detail.target.clone()?;
                let definition = surface.descriptor.definition(&target)?;
                ExecutableKey {
                    target,
                    action: definition.primary_action.clone(),
                }
            }
            _ => return None,
        }
    };
    Some(UiCommand::StandardActivate {
        context: surface.context.clone(),
        executable,
    })
}

pub(super) fn draw(
    ui: &mut egui::Ui,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    colors: &theme::Palette,
) {
    let Some(surface) = &model.navigation.standard else {
        if model.navigation.standard_route && model.navigation.unavailable.is_none() {
            ui.label("Loading view…");
        }
        return;
    };
    let pending = model.navigation.standard_refresh_pending;
    if pending && model.navigation.standard_pointer_source.is_none() {
        ui.label("Refreshing view…");
        return;
    }
    if matches!(
        surface.descriptor.view,
        StandardView::List(_) | StandardView::Grid(_)
    ) {
        let host = ui.ctx().data(|data| {
            data.get_temp::<bool>(query_edit_id().with("collection-navigation"))
                .unwrap_or(true)
        });
        ui.small(if host {
            "Collection navigation · type to search"
        } else {
            "Editing query · selection navigation returns to collection"
        });
    }
    let body_height = (ui.available_height()
        - colors.metrics.footer_height
        - (colors.metrics.item_spacing_y + colors.metrics.space_3) * 3.0
        - colors.metrics.control_height
        - if model.action_panel.is_some() {
            colors.metrics.scaled(actions_overlay_height())
                + (colors.metrics.item_spacing_y + colors.metrics.space_3)
        } else {
            0.0
        })
    .max(0.0);
    match &surface.descriptor.view {
        StandardView::List(_) => draw_results(ui, model, commands, colors, Some(body_height)),
        StandardView::Grid(grid) => {
            let scope = model.navigation.selected_scope.as_ref().map(|scope| &scope.id);
            let visible = |p: &&crikey_core::result_presentation::ItemPresentation| {
                scope.is_none_or(|scope| p.scopes.contains(scope))
            };
            let mut presentations = grid.collection.items.iter().filter(visible);
            let textures = preview_images(
                ui.ctx(),
                grid.collection
                    .items
                    .iter()
                    .filter(visible)
                    .filter_map(|p| p.image.as_ref()),
            );
            let columns = grid.columns as usize;
            let restore_id = ui.id().with((
                "standard-grid-restore",
                surface.context.session.logical_view.get(),
            ));
            let restoring = model.navigation.restoration != 0
                && ui.data(|data| data.get_temp::<u64>(restore_id)) != Some(model.navigation.restoration);
            ui.data_mut(|data| data.insert_temp(restore_id, model.navigation.restoration));
            let selection_id = ui.id().with("standard-grid-selection");
            let selection = (
                surface.context.session.surface.get(),
                surface.context.revision.get(),
                model.selected,
            );
            let follow = ui.data(|data| data.get_temp::<(u64, u64, usize)>(selection_id)) != Some(selection);
            ui.data_mut(|data| data.insert_temp(selection_id, selection));
            let keyboard_follow = follow
                && !restoring
                && !model.navigation.preserve_scroll
                && model.selected < model.rows.len();
            let mut first_visible = None;
            let restoration_spacing = ui.spacing().item_spacing.y;
            egui::ScrollArea::vertical()
                .id_salt("standard-grid")
                .max_height(body_height)
                // A fresh host keyboard choice must be visible on the next paint,
                // rather than spending another animation after the worker refresh.
                .animated(!keyboard_follow)
                .show_viewport(ui, |ui, viewport| {
                    let scroll_viewport = egui::Rect::from_min_size(
                        ui.max_rect().min + viewport.min.to_vec2(),
                        viewport.size(),
                    )
                    .intersect(ui.clip_rect());
                    let width = ((ui.available_width()
                        - ui.spacing().item_spacing.x * columns.saturating_sub(1) as f32)
                        / columns as f32)
                        .max(1.0);
                    let mut start = 0;
                    while start < model.rows.len() {
                        let (_, section_end) = crate::standard_view::section_bounds(
                            start,
                            model.rows.len(),
                            &model.navigation.section_headers,
                        );
                        let end = (start + columns).min(section_end);
                        let chunk = &model.rows[start..end];
                        if let Some(header) = model
                            .navigation
                            .section_headers
                            .iter()
                            .find(|header| header.row == start)
                        {
                            ui.strong(&header.label);
                        }
                        ui.horizontal(|ui| {
                            for (offset, row) in chunk.iter().enumerate() {
                                let index = start + offset;
                                let presentation = presentations.next();
                                let tile = ui.allocate_ui_with_layout(
                                    egui::vec2(
                                        width,
                                        colors.metrics.row_height + colors.metrics.scaled(88.0),
                                    ),
                                    egui::Layout::top_down(egui::Align::Center),
                                    |ui| {
                                        if let Some(image) = presentation.and_then(|p| p.image.as_ref()) {
                                            if let Some(texture) =
                                                textures.get(&(Arc::as_ptr(image) as usize))
                                            {
                                                ui.add(
                                                    egui::Image::new((
                                                        *texture,
                                                        egui::vec2(image.width as f32, image.height as f32),
                                                    ))
                                                    .max_size(egui::vec2(width, colors.metrics.scaled(96.0))),
                                                );
                                            }
                                        }
                                        let response =
                                            ui.selectable_label(index == model.selected, &row.label);
                                        ui.small(&row.description);
                                        response
                                    },
                                );
                                let response = tile.inner;
                                record_collection_focus(ui.ctx(), model, &response);
                                let visible_rect = tile.response.rect.intersect(scroll_viewport);
                                if first_visible.is_none()
                                    && visible_rect.width() > 0.0
                                    && visible_rect.height() > 0.0
                                {
                                    first_visible = Some(crate::LogicalScroll {
                                        row: index,
                                        offset: (scroll_viewport.top() - tile.response.rect.top()).max(0.0),
                                    });
                                }
                                if restoring
                                    && index
                                        == model
                                            .navigation
                                            .scroll
                                            .row
                                            .min(model.rows.len().saturating_sub(1))
                                {
                                    ui.scroll_to_rect(
                                        tile.response.rect.translate(egui::vec2(
                                            0.0,
                                            model.navigation.scroll.offset + restoration_spacing,
                                        )),
                                        Some(egui::Align::Min),
                                    );
                                }
                                if response.double_clicked() {
                                    if let ActionTarget::Result { owner, item } = &row.target {
                                        commands.push(UiCommand::StandardDoubleClick {
                                            context: model
                                                .navigation
                                                .standard_pointer_source
                                                .as_ref()
                                                .unwrap_or(surface)
                                                .context
                                                .clone(),
                                            executable: ExecutableKey {
                                                target: OwnedItem {
                                                    owner: owner.clone(),
                                                    item: item.clone(),
                                                },
                                                action: row
                                                    .default_action
                                                    .as_ref()
                                                    .map(|a| a.action_id.clone()),
                                            },
                                        });
                                    }
                                } else if response.clicked() && !pending {
                                    if let ActionTarget::Result { owner, item } = &row.target {
                                        commands.push(UiCommand::StandardSelect {
                                            context: surface.context.clone(),
                                            target: OwnedItem {
                                                owner: owner.clone(),
                                                item: item.clone(),
                                            },
                                        });
                                    }
                                }
                                if index == model.selected && keyboard_follow {
                                    response.scroll_to_me(None);
                                }
                            }
                        });
                        start = end;
                    }
                });
            if !restoring {
                if let Some(scroll) = first_visible {
                    commands.push(UiCommand::SetLogicalScroll(scroll));
                }
            }
        }
        StandardView::Detail(detail) => {
            preview_render::draw_standard_detail(ui, surface, detail, commands, body_height, &colors.metrics)
        }
        StandardView::Form(_) => {}
    }
    if pending {
        return;
    }
    let selected = model
        .rows
        .get(model.selected)
        .and_then(|row| match &row.target {
            ActionTarget::Result { owner, item } => Some((owner, item)),
            _ => None,
        })
        .or_else(|| match &surface.descriptor.view {
            StandardView::Detail(detail) => {
                detail.target.as_ref().map(|target| (&target.owner, &target.item))
            }
            StandardView::Form(form) => Some((&form.submit.target.owner, &form.submit.target.item)),
            _ => None,
        });
    if let Some((owner, item)) = selected {
        if let Some(definition) = surface
            .descriptor
            .definitions
            .iter()
            .find(|definition| &definition.item.plugin_id == owner && &definition.item.stable_id == item)
        {
            ui.horizontal(|ui| {
                ui.menu_button("Actions", |ui| {
                    for section in &definition.action_sections {
                        ui.strong(&section.label);
                        menu(ui, surface, definition, &section.entries, commands);
                    }
                });
                draw_navigation_hint(ui, model, crate::navigation::NavigationAction::ShowActions);
            });
        }
    }
    if model.action_panel.is_some() {
        draw_actions(ui, model, commands, colors);
    }
}

fn menu(
    ui: &mut egui::Ui,
    surface: &crate::standard_view::StandardSurface,
    definition: &crikey_core::structured_view::RouteItemDefinition,
    entries: &[ActionMenuEntry],
    commands: &mut Vec<UiCommand>,
) {
    for entry in entries {
        match entry {
            ActionMenuEntry::Submenu { label, entries, .. } => {
                ui.menu_button(label, |ui| menu(ui, surface, definition, entries, commands));
            }
            ActionMenuEntry::Action(presentation) => {
                let label = presentation
                    .action
                    .as_ref()
                    .and_then(|id| definition.action(id))
                    .map_or("Run", |action| action.label.as_str());
                let label = if presentation.destructive {
                    egui::RichText::new(label).color(ui.visuals().error_fg_color)
                } else {
                    egui::RichText::new(label)
                };
                if ui.button(label).clicked() {
                    commands.push(UiCommand::StandardActivate {
                        context: surface.context.clone(),
                        executable: ExecutableKey {
                            target: OwnedItem {
                                owner: definition.item.plugin_id.clone(),
                                item: definition.item.stable_id.clone(),
                            },
                            action: presentation.action.clone(),
                        },
                    });
                    ui.close_menu();
                }
            }
        }
    }
}

// Uses the existing native page raster upload/cache, with no URL resolution.
fn preview_images<'a>(
    context: &egui::Context,
    images: impl Iterator<Item = &'a Arc<crikey_core::preview::PreviewImage>>,
) -> HashMap<usize, egui::TextureId> {
    use std::hash::{Hash, Hasher};
    let cache = context.data_mut(|data| {
        data.get_temp_mut_or_insert_with(page_textures_id(), || {
            Arc::new(Mutex::new(PageTextures::default()))
        })
        .clone()
    });
    let mut cache = lock_recover(&cache);
    let mut previous = std::mem::take(&mut cache.by_content);
    let mut result = HashMap::new();
    for image in images {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        image.width.hash(&mut hash);
        image.height.hash(&mut hash);
        image.rgba.hash(&mut hash);
        let key = hash.finish();
        let texture = cache.by_content.entry(key).or_insert_with(|| {
            previous.remove(&key).unwrap_or_else(|| {
                context.load_texture(
                    "page-raster",
                    ColorImage::from_rgba_unmultiplied(
                        [image.width as usize, image.height as usize],
                        &image.rgba,
                    ),
                    TextureOptions::LINEAR,
                )
            })
        });
        result.insert(Arc::as_ptr(image) as usize, texture.id());
    }
    result
}
