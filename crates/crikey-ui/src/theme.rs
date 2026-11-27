use std::collections::BTreeMap;

use egui::{vec2, Color32, FontFamily, FontId, Margin, Rounding, Shadow, Stroke, Style, TextStyle, Visuals};

use crate::appearance::{InterfaceSize, ResolvedAppearance, ResolvedTheme, ResultDensity};

pub(crate) const DEFAULT_WINDOW_WIDTH: u32 = 720;
pub(crate) const DEFAULT_WINDOW_HEIGHT: u32 = 520;
pub(crate) const MIN_WINDOW_WIDTH: u32 = 480;
/// The window's height when there is nothing below the query field.
///
/// An untyped launcher shows the query field and the footer and nothing else,
/// so the window is exactly that tall: a full-height window standing empty
/// over the desktop is what the first Windows tester read as the launcher
/// being broken. It doubles as the minimum a user may drag the window to,
/// because a minimum above it would keep the compact window from compacting.
///
/// It is the whole compact frame, both panel margins included:
/// [`PANEL_MARGIN`] + [`FIELD_HEIGHT`] + the block gap +
/// [`FOOTER_HEIGHT`] + [`PANEL_MARGIN`]. `desired_window_height` adds the
/// result list to it rather than re-deriving the field and the footer, and
/// `a_compact_window_leaves_room_for_the_field_and_the_footer` lays the
/// compact frame out and fails if this stops covering what it draws.
pub(crate) const COMPACT_WINDOW_HEIGHT: u32 = 94;
pub(crate) const MIN_WINDOW_HEIGHT: u32 = COMPACT_WINDOW_HEIGHT;

pub(crate) const SPACE_1: f32 = 4.0;
pub(crate) const SPACE_2: f32 = 8.0;
pub(crate) const SPACE_3: f32 = 12.0;
pub(crate) const SPACE_8: f32 = 32.0;

/// The vertical gap egui puts between two things stacked in a vertical layout.
///
/// Named because it is drawn whether or not anybody asked for it, so every
/// height the launcher computes has to account for it: see `BLOCK_GAP`,
/// `STATUS_BLOCK_HEIGHT` and `actions_overlay_height`.
pub(crate) const ITEM_SPACING_Y: f32 = SPACE_1;

/// The central panel's inner margin, in logical pixels.
///
/// The launcher is one column and the window is only as wide as it needs to
/// be, so the margin is a frame around the content rather than a gutter: it is
/// kept small on purpose. It is also the last thing under the footer, which is
/// why the window-height arithmetic ends with it.
pub(crate) const PANEL_MARGIN: f32 = SPACE_3;

/// The height of the query field, in logical pixels.
///
/// Exactly one line of [`TEXT_QUERY`] inside the field's own padding: the
/// field is a single filled pill with no border, so any slack in it reads as
/// the field being mis-sized rather than as breathing room.
pub(crate) const FIELD_HEIGHT: f32 = 40.0;

/// The height of one interactive control, in logical pixels
/// (`Spacing::interact_size.y`).
///
/// Every button in the action list is this tall, which makes it part of
/// `actions_overlay_height`. The footer no longer contains one: `Settings
/// Ctrl+,` is set in the footer's own small type, so `STATUS_BLOCK_HEIGHT` is
/// [`FOOTER_HEIGHT`] instead.
pub(crate) const CONTROL_HEIGHT: f32 = 24.0;

/// The height of one result row, in logical pixels.
///
/// Every row is this tall whether or not it carries a description, so the list
/// is a regular column and the window height is arithmetic rather than a
/// measurement of a frame that has already been laid out inside the window it
/// is supposed to be sizing.
///
/// It is exactly what the two lines a row draws need: egui lays one line of
/// [`TEXT_LABEL`] out 24 px tall and one of [`TEXT_SMALL`] 18, which with
/// [`ROW_LINE_GAP`] between them and [`ROW_PAD_Y`] above and below comes to
/// this. `every_result_row_matches_the_pinned_row_metrics` lays real rows out
/// and fails if the two ever disagree, so this number is measured rather than
/// chosen.
pub(crate) const ROW_HEIGHT: f32 = 52.0;

/// The vertical gap between two result rows, in logical pixels.
///
/// A hairline: the rows are a list rather than a stack of cards, so what
/// separates them is the leading of their own text, not a gutter between
/// boxes. Enough that two adjacent selections would not touch, and no more.
pub(crate) const ROW_GAP: f32 = 2.0;

/// A result row's horizontal and vertical padding, in logical pixels.
///
/// Part of [`ROW_HEIGHT`]: the row's content is `ROW_HEIGHT - 2 * ROW_PAD_Y`
/// tall, which is what `draw_result_row` reserves.
pub(crate) const ROW_PAD_X: f32 = SPACE_2;
pub(crate) const ROW_PAD_Y: f32 = SPACE_1;

/// The gap between a row's label and its muted metadata line, in logical
/// pixels. Two lines of one row belong together, so it is tighter than
/// [`ITEM_SPACING_Y`], which separates unrelated things.
pub(crate) const ROW_LINE_GAP: f32 = 2.0;

pub(crate) const RADIUS_SMALL: f32 = 4.0;
pub(crate) const RADIUS_MEDIUM: f32 = 8.0;

/// The radius of the launcher window's own corners, in logical pixels.
///
/// Concentric with the query field rather than a size of its own: an arc of
/// [`RADIUS_MEDIUM`] set [`PANEL_MARGIN`] inside this one begins turning on
/// exactly the same horizontal and vertical lines, so the window's curve runs
/// parallel to the field's at a constant distance. Any other outer radius
/// starts its turn early or late against the field it encloses, which is what
/// reads as the corner being wrong even when both are rounded.
pub(crate) const RADIUS_WINDOW: f32 = RADIUS_MEDIUM + PANEL_MARGIN;

/// The edge of a result row's icon slot, in logical pixels.
///
/// The slot is a fixed square whether or not an icon fills it, so the label
/// column starts at the same x on every row of every list. Decoded icons are
/// requested at [`crikey_platform::DEFAULT_ICON_SIZE`], which is larger, so a
/// themed raster is downscaled rather than stretched.
pub(crate) const ICON_SIZE: f32 = 28.0;

pub(crate) const TEXT_SMALL: f32 = 12.0;
pub(crate) const TEXT_BODY: f32 = 14.0;
pub(crate) const TEXT_LABEL: f32 = 16.0;
pub(crate) const TEXT_QUERY: f32 = 24.0;

/// The height of the footer, in logical pixels: one line of [`TEXT_SMALL`]
/// and nothing else.
///
/// The footer holds no buttons any more, so it is as tall as its own text
/// rather than as tall as a control. Left at the control height it would
/// reserve five pixels of empty canvas under the hint line, and the window
/// height is arithmetic over this, so that strip would appear in every window
/// the launcher opens.
///
/// Measured rather than chosen — egui lays [`TEXT_SMALL`] out 14 px tall —
/// and `a_compact_window_leaves_room_for_the_field_and_the_footer` lays a real
/// footer out and fails if the two ever disagree.
pub(crate) const FOOTER_HEIGHT: f32 = 14.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Palette {
    pub metrics: Metrics,
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

/// The launcher's colours.
///
/// Three surface tiers and no borders: [`Palette::canvas`] is the window,
/// [`Palette::surface`] is a sheet standing on it -- the query field, the
/// action list, the settings surface -- and [`Palette::raised`] is a control
/// standing on a sheet. Dark tiers get lighter; light tiers get darker, so
/// chrome remains distinguishable without adding borders.
pub(crate) fn base_palette(theme: ResolvedTheme) -> Palette {
    let metrics = Metrics::new(InterfaceSize::Default, ResultDensity::Default);
    if theme == ResolvedTheme::Light {
        return Palette {
            metrics,
            canvas: Color32::from_rgb(247, 248, 250),
            surface: Color32::from_rgb(237, 240, 244),
            raised: Color32::from_rgb(225, 229, 235),
            input: Color32::from_rgb(255, 255, 255),
            border: Color32::from_rgb(151, 160, 173),
            text: Color32::from_rgb(25, 30, 38),
            text_muted: Color32::from_rgb(72, 81, 94),
            accent: Color32::from_rgb(133, 76, 0),
            accent_soft: Color32::from_rgb(245, 224, 190),
            warning: Color32::from_rgb(133, 83, 0),
            error: Color32::from_rgb(169, 42, 42),
        };
    }
    Palette {
        metrics,
        canvas: Color32::from_rgb(20, 22, 26),
        surface: Color32::from_rgb(34, 38, 45),
        raised: Color32::from_rgb(46, 51, 59),
        input: Color32::from_rgb(23, 26, 31),
        border: Color32::from_rgb(57, 64, 74),
        text: Color32::from_rgb(235, 238, 242),
        text_muted: Color32::from_rgb(158, 167, 179),
        accent: Color32::from_rgb(232, 174, 88),
        accent_soft: Color32::from_rgb(70, 56, 36),
        warning: Color32::from_rgb(226, 184, 102),
        error: Color32::from_rgb(240, 140, 140),
    }
}

/// Install only when the caller-owned resolved appearance changes.
pub(crate) fn install(context: &egui::Context, resolved: &ResolvedAppearance) {
    let colors = &resolved.palette;
    let metrics = resolved.metrics();
    let mut style = Style {
        text_styles: BTreeMap::from([
            (
                TextStyle::Small,
                FontId::new(metrics.text_small, FontFamily::Proportional),
            ),
            (
                TextStyle::Body,
                FontId::new(metrics.text_body, FontFamily::Proportional),
            ),
            (
                TextStyle::Button,
                FontId::new(metrics.text_body, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(metrics.text_query, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(metrics.text_small, FontFamily::Monospace),
            ),
        ]),
        ..Style::default()
    };
    style.spacing.item_spacing = vec2(metrics.space_2, metrics.item_spacing_y);
    style.spacing.window_margin = Margin::same(metrics.panel_margin);
    style.spacing.button_padding = vec2(metrics.space_2, metrics.space_1);
    style.spacing.interact_size = vec2(metrics.space_8, metrics.control_height);
    style.spacing.text_edit_width = metrics.default_window_width - metrics.space_8;

    let mut visuals = match resolved.theme {
        ResolvedTheme::Light => Visuals::light(),
        ResolvedTheme::Dark => Visuals::dark(),
    };
    visuals.override_text_color = Some(colors.text);
    visuals.panel_fill = colors.canvas;
    visuals.window_fill = colors.surface;
    visuals.window_stroke = Stroke::new(1.0_f32, colors.border);
    visuals.window_rounding = Rounding::same(metrics.radius_medium);
    visuals.window_shadow = Shadow::NONE;
    visuals.popup_shadow = Shadow::NONE;
    visuals.extreme_bg_color = colors.input;
    visuals.faint_bg_color = colors.raised;
    visuals.warn_fg_color = colors.warning;
    visuals.error_fg_color = colors.error;
    visuals.selection.bg_fill = colors.accent_soft;
    visuals.selection.stroke = Stroke::new(1.0_f32, colors.accent);
    // No widget borders. A launcher this small is read as a whole rather than
    // scanned, so a stroke around every control is noise the fills already
    // carry: `Palette` steps each surface tier away from the one beneath it.
    // The tooltip keeps `window_stroke`, which is the one surface that floats
    // over content it does not belong to.
    visuals.widgets.noninteractive.bg_fill = colors.surface;
    visuals.widgets.noninteractive.weak_bg_fill = colors.surface;
    visuals.widgets.noninteractive.bg_stroke = Stroke::NONE;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, colors.text);
    visuals.widgets.noninteractive.rounding = Rounding::same(metrics.radius_small);
    visuals.widgets.inactive.bg_fill = colors.raised;
    visuals.widgets.inactive.weak_bg_fill = colors.raised;
    visuals.widgets.inactive.bg_stroke = Stroke::NONE;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, colors.text);
    visuals.widgets.inactive.rounding = Rounding::same(metrics.radius_small);
    visuals.widgets.hovered.bg_fill = colors.accent_soft;
    visuals.widgets.hovered.weak_bg_fill = colors.accent_soft;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, colors.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, colors.text);
    visuals.widgets.hovered.rounding = Rounding::same(metrics.radius_small);
    visuals.widgets.active = visuals.widgets.hovered;
    visuals.widgets.open = visuals.widgets.hovered;
    style.visuals = visuals;

    // The host resolver owns System handling too. Leaving egui in System mode
    // lets a later RawInput observation select an unconfigured style slot.
    context.set_theme(match resolved.theme {
        ResolvedTheme::Light => egui::Theme::Light,
        ResolvedTheme::Dark => egui::Theme::Dark,
    });
    context.set_style(style);
}

/// Fully resolved logical geometry. OS DPI remains exclusively platform-owned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Metrics {
    pub scale: f32,
    pub default_window_width: f32,
    pub default_window_height: f32,
    pub min_window_width: f32,
    pub min_window_height: f32,
    pub compact_window_height: f32,
    pub space_1: f32,
    pub space_2: f32,
    pub space_3: f32,
    pub space_8: f32,
    pub item_spacing_y: f32,
    pub panel_margin: f32,
    pub field_height: f32,
    pub control_height: f32,
    pub row_height: f32,
    pub row_gap: f32,
    pub row_pad_x: f32,
    pub row_pad_y: f32,
    pub row_line_gap: f32,
    pub radius_small: f32,
    pub radius_medium: f32,
    pub radius_window: f32,
    pub icon_size: f32,
    pub text_small: f32,
    pub text_body: f32,
    pub text_label: f32,
    pub text_query: f32,
    pub footer_height: f32,
}
impl Metrics {
    pub(crate) fn new(size: InterfaceSize, density: ResultDensity) -> Self {
        let scale = match size {
            InterfaceSize::Small => 0.9,
            InterfaceSize::Default => 1.0,
            InterfaceSize::Large => 1.15,
        };
        let (row_pad_y, row_gap) = match density {
            ResultDensity::Compact => (ROW_PAD_Y - 1.0, 0.0),
            ResultDensity::Default => (ROW_PAD_Y, ROW_GAP),
            ResultDensity::Comfortable => (ROW_PAD_Y + 4.0, ROW_GAP + 2.0),
        };
        // Font line boxes round conservatively in logical coordinates. Keep
        // footer and frame reservations coherent without applying OS DPI here.
        let footer_height = (FOOTER_HEIGHT * scale).ceil();
        let footer_rounding = footer_height - FOOTER_HEIGHT * scale;
        Self {
            scale,
            default_window_width: DEFAULT_WINDOW_WIDTH as f32 * scale,
            default_window_height: DEFAULT_WINDOW_HEIGHT as f32 * scale,
            min_window_width: MIN_WINDOW_WIDTH as f32 * scale,
            min_window_height: MIN_WINDOW_HEIGHT as f32 * scale + footer_rounding,
            compact_window_height: COMPACT_WINDOW_HEIGHT as f32 * scale + footer_rounding,
            space_1: SPACE_1 * scale,
            space_2: SPACE_2 * scale,
            space_3: SPACE_3 * scale,
            space_8: SPACE_8 * scale,
            item_spacing_y: ITEM_SPACING_Y * scale,
            panel_margin: PANEL_MARGIN * scale,
            field_height: FIELD_HEIGHT * scale,
            control_height: CONTROL_HEIGHT * scale,
            row_height: (ROW_HEIGHT + 2.0 * (row_pad_y - ROW_PAD_Y)) * scale,
            row_gap: row_gap * scale,
            row_pad_x: ROW_PAD_X * scale,
            row_pad_y: row_pad_y * scale,
            row_line_gap: ROW_LINE_GAP * scale,
            radius_small: RADIUS_SMALL * scale,
            radius_medium: RADIUS_MEDIUM * scale,
            radius_window: RADIUS_WINDOW * scale,
            icon_size: ICON_SIZE * scale,
            text_small: TEXT_SMALL * scale,
            text_body: TEXT_BODY * scale,
            text_label: TEXT_LABEL * scale,
            text_query: TEXT_QUERY * scale,
            footer_height,
        }
    }
    pub(crate) fn scaled(&self, logical: f32) -> f32 {
        logical * self.scale
    }
}
