use std::collections::BTreeMap;

use egui::{vec2, Color32, FontFamily, FontId, Margin, Rounding, Shadow, Stroke, Style, TextStyle, Visuals};

pub(crate) const DEFAULT_WINDOW_WIDTH: u32 = 720;
pub(crate) const DEFAULT_WINDOW_HEIGHT: u32 = 520;
pub(crate) const MIN_WINDOW_WIDTH: u32 = 480;
pub(crate) const MIN_WINDOW_HEIGHT: u32 = 320;

pub(crate) const SPACE_1: f32 = 4.0;
pub(crate) const SPACE_2: f32 = 8.0;
pub(crate) const SPACE_3: f32 = 12.0;
pub(crate) const SPACE_4: f32 = 16.0;
pub(crate) const SPACE_6: f32 = 24.0;
pub(crate) const SPACE_8: f32 = 32.0;

pub(crate) const RADIUS_SMALL: f32 = 4.0;
pub(crate) const RADIUS_MEDIUM: f32 = 8.0;

pub(crate) const TEXT_SMALL: f32 = 12.0;
pub(crate) const TEXT_BODY: f32 = 14.0;
pub(crate) const TEXT_LABEL: f32 = 16.0;
pub(crate) const TEXT_QUERY: f32 = 24.0;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Palette {
    pub canvas: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub input: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub warning: Color32,
    pub error: Color32,
}

pub(crate) fn palette() -> Palette {
    Palette {
        canvas: Color32::from_rgb(20, 22, 26),
        surface: Color32::from_rgb(27, 30, 35),
        raised: Color32::from_rgb(35, 39, 45),
        input: Color32::from_rgb(23, 26, 31),
        border: Color32::from_rgb(57, 64, 74),
        text: Color32::from_rgb(235, 238, 242),
        text_muted: Color32::from_rgb(158, 167, 179),
        accent: Color32::from_rgb(232, 174, 88),
        accent_soft: Color32::from_rgb(63, 50, 33),
        warning: Color32::from_rgb(226, 184, 102),
        error: Color32::from_rgb(222, 112, 112),
    }
}

pub(crate) fn install(context: &egui::Context) {
    let colors = palette();
    let mut style = Style {
        text_styles: BTreeMap::from([
            (
                TextStyle::Small,
                FontId::new(TEXT_SMALL, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(TEXT_BODY, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(TEXT_BODY, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(TEXT_QUERY, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(TEXT_SMALL, FontFamily::Monospace),
            ),
        ]),
        ..Style::default()
    };
    style.spacing.item_spacing = vec2(SPACE_2, SPACE_2);
    style.spacing.window_margin = Margin::same(SPACE_4);
    style.spacing.button_padding = vec2(SPACE_3, SPACE_2);
    style.spacing.interact_size = vec2(SPACE_8, SPACE_8);
    style.spacing.text_edit_width = DEFAULT_WINDOW_WIDTH as f32 - SPACE_8;

    let mut visuals = Visuals::dark();
    visuals.override_text_color = Some(colors.text);
    visuals.panel_fill = colors.canvas;
    visuals.window_fill = colors.surface;
    visuals.window_stroke = Stroke::new(1.0_f32, colors.border);
    visuals.window_rounding = Rounding::same(RADIUS_MEDIUM);
    visuals.window_shadow = Shadow::NONE;
    visuals.popup_shadow = Shadow::NONE;
    visuals.extreme_bg_color = colors.input;
    visuals.faint_bg_color = colors.raised;
    visuals.warn_fg_color = colors.warning;
    visuals.error_fg_color = colors.error;
    visuals.selection.bg_fill = colors.accent_soft;
    visuals.selection.stroke = Stroke::new(1.0_f32, colors.accent);
    visuals.widgets.noninteractive.bg_fill = colors.surface;
    visuals.widgets.noninteractive.weak_bg_fill = colors.surface;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, colors.border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, colors.text);
    visuals.widgets.noninteractive.rounding = Rounding::same(RADIUS_SMALL);
    visuals.widgets.inactive.bg_fill = colors.raised;
    visuals.widgets.inactive.weak_bg_fill = colors.raised;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, colors.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, colors.text);
    visuals.widgets.inactive.rounding = Rounding::same(RADIUS_SMALL);
    visuals.widgets.hovered.bg_fill = colors.accent_soft;
    visuals.widgets.hovered.weak_bg_fill = colors.accent_soft;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, colors.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, colors.text);
    visuals.widgets.hovered.rounding = Rounding::same(RADIUS_SMALL);
    visuals.widgets.active = visuals.widgets.hovered;
    visuals.widgets.open = visuals.widgets.hovered;
    style.visuals = visuals;

    context.set_style(style);
}
