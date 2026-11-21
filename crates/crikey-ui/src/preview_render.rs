//! Typed preview layout; all producer work is complete before paint.
use super::*;

pub(super) fn retains_image(model: &ViewModel) -> bool {
    if model.navigation.standard_refresh_pending {
        return false;
    }
    if model.settings_open || model.page.is_some() {
        return false;
    }
    if let Some(surface) = &model.navigation.standard {
        return matches!(&surface.descriptor.view, crikey_core::structured_view::StandardView::Detail(detail) if detail.content().image.is_some());
    }
    shows_results(model)
        && matches!(&model.preview.status, Some(crate::PreviewStatus::Content(prepared)) if prepared.content.image.is_some())
}

/// One image per context, independent of the split/narrow widget hierarchy.
/// A weak identity never keeps the producer's CPU pixels alive.
struct PreviewTexture {
    content: std::sync::Weak<crikey_core::PreviewContent>,
    handle: TextureHandle,
}

impl fmt::Debug for PreviewTexture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreviewTexture")
            .field("id", &self.handle.id())
            .finish()
    }
}

fn texture_id() -> egui::Id {
    egui::Id::new("crikey-preview-texture")
}

pub(super) fn release_texture(context: &egui::Context) -> Option<egui::TextureId> {
    context
        .data_mut(|data| {
            let texture = data.get_temp::<Arc<Mutex<PreviewTexture>>>(texture_id());
            data.remove::<Arc<Mutex<PreviewTexture>>>(texture_id());
            texture
        })
        .map(|texture| {
            let id = lock_recover(&texture).handle.id();
            id
        })
}

fn image_texture(context: &egui::Context, content: &Arc<crikey_core::PreviewContent>) -> egui::TextureId {
    if let Some(texture) = context.data(|data| data.get_temp::<Arc<Mutex<PreviewTexture>>>(texture_id())) {
        let texture = lock_recover(&texture);
        if texture.content.as_ptr() == Arc::as_ptr(content) {
            return texture.handle.id();
        }
    }
    let image = content.image.as_ref().expect("visible preview image");
    let handle = context.load_texture(
        "preview-image",
        ColorImage::from_rgba_unmultiplied([image.width as usize, image.height as usize], &image.rgba),
        TextureOptions::LINEAR,
    );
    let id = handle.id();
    context.data_mut(|data| {
        data.insert_temp(
            texture_id(),
            Arc::new(Mutex::new(PreviewTexture {
                content: Arc::downgrade(content),
                handle,
            })),
        )
    });
    id
}

pub(super) fn draw_results_and_preview(
    ui: &mut egui::Ui,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    colors: &theme::Palette,
) {
    if model.preview.status.is_none() {
        release_texture(ui.ctx());
        draw_results(ui, model, commands, colors, None);
        return;
    }
    let narrow = ui.available_width() < colors.metrics.scaled(600.0);
    if narrow {
        if model.preview.detail || model.preview.focused {
            if ui.button("Back to results").clicked() {
                commands.push(UiCommand::LeavePreview);
            }
            draw_preview(ui, model, commands, colors, true);
        } else {
            release_texture(ui.ctx());
            if ui.button("Show detail").clicked() {
                commands.push(UiCommand::ShowPreviewDetail);
            }
            draw_results(ui, model, commands, colors, None);
        }
        return;
    }
    let available = ui.available_rect_before_wrap();
    let overlay = if model.action_panel.is_some() {
        (colors.metrics.item_spacing_y + colors.metrics.space_3)
            + colors.metrics.scaled(actions_overlay_height())
    } else {
        0.0
    };
    let height = (available.height()
        - (colors.metrics.item_spacing_y + colors.metrics.space_3)
        - colors.metrics.footer_height
        - overlay)
        .max(0.0);
    let width = available.width();
    let id = ui.id().with("preview-divider");
    let ratio = ui
        .data(|data| data.get_temp::<f32>(id))
        .unwrap_or(model.preview.ratio);
    let left = (width - colors.metrics.scaled(10.0)) * ratio;
    let divider = egui::Rect::from_min_size(
        available.min + vec2(left, 0.0),
        vec2(colors.metrics.scaled(10.0), height),
    );
    let response = ui.interact(divider, id, egui::Sense::drag());
    ui.painter().line_segment(
        [divider.center_top(), divider.center_bottom()],
        Stroke::new(1.0, colors.text_muted),
    );
    if response.dragged() {
        if let Some(pointer) = response.interact_pointer_pos() {
            ui.data_mut(|data| {
                data.insert_temp(
                    id,
                    ((pointer.x - available.min.x) / (width - colors.metrics.scaled(10.0))).clamp(0.3, 0.7),
                )
            });
            ui.ctx().request_repaint();
        }
    }
    if response.drag_stopped() {
        if let Some(ratio) = ui.data_mut(|data| data.remove_temp::<f32>(id)) {
            commands.push(UiCommand::SetPreviewRatio(ratio));
        }
    }
    let list_rect = egui::Rect::from_min_size(available.min, vec2(left, height));
    let mut list_ui = ui.new_child(UiBuilder::new().max_rect(list_rect).id_salt("preview-results"));
    list_ui.set_clip_rect(list_ui.clip_rect().intersect(list_rect));
    draw_results(&mut list_ui, model, commands, colors, Some(height));
    let preview_rect = egui::Rect::from_min_max(divider.right_top(), available.min + vec2(width, height));
    let mut preview_ui = ui.new_child(UiBuilder::new().max_rect(preview_rect).id_salt("preview-pane"));
    preview_ui.set_clip_rect(preview_ui.clip_rect().intersect(preview_rect));
    draw_preview(&mut preview_ui, model, commands, colors, false);
    ui.allocate_space(vec2(width, height));
}

fn draw_preview(
    ui: &mut egui::Ui,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    colors: &theme::Palette,
    reserve_footer: bool,
) {
    let Some(status) = &model.preview.status else {
        return;
    };
    if !matches!(status, crate::PreviewStatus::Content(prepared) if prepared.content.image.is_some()) {
        release_texture(ui.ctx());
    }
    ui.push_id("typed-preview", |ui| {
        ui.horizontal(|ui| {
            draw_navigation_hint(ui, model, crate::navigation::NavigationAction::TogglePreview);
            ui.strong("Detail");
            let response = ui.button(if model.preview.focused { "Search" } else { "Focus" });
            let focus_id = ui.id().with("preview-had-focus");
            let was_focused = ui.data(|data| data.get_temp::<bool>(focus_id)).unwrap_or(false);
            if model.preview.focused && !was_focused {
                response.request_focus();
            }
            ui.data_mut(|data| data.insert_temp(focus_id, model.preview.focused));
            if response.clicked() {
                commands.push(if model.preview.focused {
                    UiCommand::LeavePreview
                } else {
                    UiCommand::FocusPreview
                });
            }
        });
        let height = (ui.available_height()
            - if reserve_footer {
                (colors.metrics.item_spacing_y + colors.metrics.space_3) + colors.metrics.footer_height
            } else {
                0.0
            })
        .max(0.0);
        let scroll_id = ui.id().with("preview-key-scroll");
        let mut offset = ui.data(|data| data.get_temp::<f32>(scroll_id)).unwrap_or(0.0);
        if model.preview.focused {
            ui.input_mut(|input| {
                for (key, distance) in [
                    (egui::Key::ArrowDown, colors.metrics.scaled(40.0)),
                    (egui::Key::ArrowUp, -colors.metrics.scaled(40.0)),
                    (egui::Key::PageDown, height * 0.8),
                    (egui::Key::PageUp, -height * 0.8),
                ] {
                    if input.consume_key(egui::Modifiers::NONE, key) {
                        offset = (offset + distance).max(0.0);
                    }
                }
            });
        }
        let output = egui::ScrollArea::vertical()
            .id_salt("preview-scroll")
            .vertical_scroll_offset(offset)
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                match status {
                    crate::PreviewStatus::Loading => {
                        ui.spinner();
                        ui.label("Loading preview…");
                    }
                    crate::PreviewStatus::Empty => {
                        ui.label("No preview content");
                    }
                    crate::PreviewStatus::Unavailable(reason) => {
                        ui.label("Preview unavailable");
                        ui.label(reason);
                    }
                    crate::PreviewStatus::Error(reason) => {
                        ui.colored_label(colors.warning, "Preview failed");
                        ui.label(reason);
                    }
                    crate::PreviewStatus::Content(prepared) => {
                        draw_prepared(
                            ui,
                            prepared,
                            model.preview.link_target.as_ref().map(LinkTarget::Preview),
                            commands,
                            &colors.metrics,
                        );
                    }
                }
            });
        ui.data_mut(|data| data.insert_temp(scroll_id, output.state.offset.y));
    });
}

#[derive(Clone, Copy)]
enum LinkTarget<'a> {
    Preview(&'a crate::PreviewLinkTarget),
    Standard(&'a crate::standard_view::StandardSurface),
}

pub(super) fn draw_standard_detail(
    ui: &mut egui::Ui,
    surface: &crate::standard_view::StandardSurface,
    detail: &crikey_core::structured_view::DetailView,
    commands: &mut Vec<UiCommand>,
    height: f32,
    metrics: &theme::Metrics,
) {
    let prepared = crate::PreparedPreview {
        content: detail.content().clone(),
        blocks: detail.blocks().clone(),
    };
    egui::ScrollArea::vertical()
        .id_salt("standard-detail")
        .max_height(height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            draw_prepared(
                ui,
                &prepared,
                Some(LinkTarget::Standard(surface)),
                commands,
                metrics,
            );
        });
}

fn draw_prepared(
    ui: &mut egui::Ui,
    prepared: &crate::PreparedPreview,
    target: Option<LinkTarget<'_>>,
    commands: &mut Vec<UiCommand>,
    metrics: &theme::Metrics,
) {
    let content = &prepared.content;
    if let Some(crikey_core::PreviewText::Plain(text)) = &content.text {
        ui.add(egui::Label::new(text).wrap());
    }
    for block in prepared.blocks.iter() {
        draw_markdown_block(ui, block, target, commands, metrics);
    }
    if let Some(image) = &content.image {
        let texture = image_texture(ui.ctx(), content);
        ui.add(
            egui::Image::new((texture, vec2(image.width as f32, image.height as f32)))
                .max_width(ui.available_width()),
        );
    }
    for value in &content.values {
        ui.label(RichText::new(&value.label).strong());
        if value.kind == crikey_core::PreviewValueKind::Link {
            preview_link(ui, RichText::new(&value.value), &value.value, target, commands);
        } else {
            ui.add(egui::Label::new(&value.value).wrap());
        }
    }
}
fn draw_markdown_block(
    ui: &mut egui::Ui,
    block: &crate::PreviewBlock,
    target: Option<LinkTarget<'_>>,
    commands: &mut Vec<UiCommand>,
    metrics: &theme::Metrics,
) {
    use crate::PreviewBlockKind;
    if block.kind == PreviewBlockKind::Rule {
        ui.separator();
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        if block.kind == PreviewBlockKind::Bullet {
            ui.label("•  ");
        }
        for span in &block.spans {
            let mut text = RichText::new(&span.text);
            if let PreviewBlockKind::Heading(level) = block.kind {
                text = text
                    .size(metrics.scaled((25.0 - f32::from(level) * 2.0).max(15.0)))
                    .strong();
            }
            if span.strong {
                text = text.strong();
            }
            if span.emphasis {
                text = text.italics();
            }
            if span.code {
                text = text.code();
            }
            if let Some(url) = &span.link {
                preview_link(ui, text, url, target, commands);
            } else {
                ui.add(egui::Label::new(text).wrap());
            }
        }
    });
    ui.add_space(metrics.space_1);
}

fn preview_link(
    ui: &mut egui::Ui,
    text: RichText,
    url: &str,
    target: Option<LinkTarget<'_>>,
    commands: &mut Vec<UiCommand>,
) {
    let text = text.color(ui.visuals().hyperlink_color).underline();
    if ui
        .add_enabled(target.is_some(), egui::Button::new(text).frame(false))
        .on_hover_text(url)
        .clicked()
    {
        match target {
            Some(LinkTarget::Preview(target)) => commands.push(UiCommand::OpenPreviewLink {
                target: Box::new(target.clone()),
                url: url.to_owned(),
            }),
            Some(LinkTarget::Standard(surface)) => commands.push(UiCommand::StandardLink {
                context: surface.context.clone(),
                url: url.to_owned(),
            }),
            None => {}
        }
    }
}
