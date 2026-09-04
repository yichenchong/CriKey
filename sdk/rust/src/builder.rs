//! Fluent item, action and page builders (spec 10.1, 10.4, 32.3).

use std::collections::BTreeMap;

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, NodeRole,
    NodeShape, PageColor, PageError, PageFrame, PageImage, PageNode, PluginId, MAX_PAGE_IMAGE_BYTES,
    MAX_PAGE_IMAGE_EDGE,
};

use crate::PagePalette;

/// Builds a core [`Item`] with the plugin-author defaults (spec 10.1–10.3).
#[must_use = "call ItemBuilder::build to create the item"]
#[derive(Debug, Clone)]
pub struct ItemBuilder {
    stable_id: String,
    label: String,
    description: String,
    target: String,
    category: Category,
    score_hint: i32,
    search_terms: Vec<String>,
    metadata: BTreeMap<String, String>,
    actions: Vec<Action>,
    icon_reference: Option<String>,
}

impl ItemBuilder {
    /// Starts an item with a stable identifier and display label.
    pub fn new(stable_id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            stable_id: stable_id.into(),
            label: label.into(),
            description: String::new(),
            target: String::new(),
            category: Category::PluginDefined("plugin-defined".to_owned()),
            score_hint: 0,
            search_terms: Vec::new(),
            metadata: BTreeMap::new(),
            actions: Vec::new(),
            icon_reference: None,
        }
    }

    /// Sets the launch target.
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = target.into();
        self
    }

    /// Sets the display description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the item category.
    pub fn category(mut self, category: Category) -> Self {
        self.category = category;
        self
    }

    /// Sets the ranker's score hint.
    pub fn score_hint(mut self, score_hint: i32) -> Self {
        self.score_hint = score_hint;
        self
    }

    /// Adds one searchable term.
    pub fn search_term(mut self, search_term: impl Into<String>) -> Self {
        self.search_terms.push(search_term.into());
        self
    }

    /// Inserts metadata; a repeated key replaces the previous value.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Adds one action.
    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }

    /// Sets the icon reference.
    pub fn icon(mut self, icon_reference: impl Into<String>) -> Self {
        self.icon_reference = Some(icon_reference.into());
        self
    }

    /// Consumes the builder and produces the core item.
    pub fn build(self) -> Item {
        Item {
            stable_id: ItemId(self.stable_id),
            plugin_id: PluginId(String::new()),
            category: self.category,
            label: self.label,
            description: self.description,
            target: self.target,
            search_terms: self.search_terms,
            icon_reference: self.icon_reference,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: self.score_hint,
            metadata: self.metadata,
            actions: self.actions,
        }
    }
}

/// Builds an action for an item (spec 10.4).
#[must_use = "call ActionBuilder::build to create the action"]
#[derive(Debug, Clone)]
pub struct ActionBuilder {
    action_id: String,
    label: String,
    description: String,
    icon_reference: Option<String>,
    applicable_categories: Vec<Category>,
    execution_policy: ExecutionPolicy,
}

impl ActionBuilder {
    /// Starts an action with a stable identifier and display label.
    pub fn new(action_id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            action_id: action_id.into(),
            label: label.into(),
            description: String::new(),
            icon_reference: None,
            applicable_categories: Vec::new(),
            execution_policy: ExecutionPolicy::Plugin,
        }
    }

    /// Sets the action description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the action icon reference.
    pub fn icon(mut self, icon_reference: impl Into<String>) -> Self {
        self.icon_reference = Some(icon_reference.into());
        self
    }

    /// Restricts this action to items of `category` (spec 10.4). Repeatable;
    /// leaving it unset means the action applies to any item the plugin
    /// returns.
    pub fn applicable_category(mut self, category: Category) -> Self {
        self.applicable_categories.push(category);
        self
    }

    /// Selects host-mediated execution instead of plugin execution.
    pub fn host_mediated(mut self) -> Self {
        self.execution_policy = ExecutionPolicy::HostMediated;
        self
    }

    /// Consumes the builder and produces the core action.
    pub fn build(self) -> Action {
        Action {
            action_id: ActionId(self.action_id),
            label: self.label,
            description: self.description,
            applicable_categories: self.applicable_categories,
            icon_reference: self.icon_reference,
            execution_policy: self.execution_policy,
        }
    }
}

/// The host's body text size, used to estimate where a label sits when the
/// node leaves [`PageNode::text_size`] at zero.
const HOST_BODY_TEXT_SIZE: f32 = 14.0;

/// Fraction of the text size one glyph advances, averaged over a proportional
/// face. Font metrics do not cross the plugin boundary - the host lays out no
/// glyphs and the plugin owns every coordinate - so a centred label is centred
/// from an estimate. A long proportional string therefore sits a pixel or two
/// off centre, which is the price of not shipping a font engine to every
/// plugin author.
const AVERAGE_GLYPH_ADVANCE: f32 = 0.5;

/// Height of one line as a multiple of the text size, matching the host's
/// single-line galley closely enough to centre a control's label vertically.
const LINE_HEIGHT: f32 = 1.25;

/// Size of a page heading: above the launcher's body and label text so a
/// page's own title reads as a heading, below its query row so a page never
/// competes with the launcher's own chrome.
const HEADING_TEXT_SIZE: f32 = 20.0;

/// Corner radius shared by the page controls, so a page built from these
/// helpers looks like one surface rather than an assortment.
const CONTROL_ROUNDING: f32 = 6.0;

/// Side of a checkbox indicator, and therefore the height of a checkbox row.
const CHECKBOX_EDGE: f32 = 18.0;

/// Gap between a checkbox indicator and its text.
const CHECKBOX_LABEL_GAP: f32 = 10.0;

/// Inset of a text field's contents from its frame.
const FIELD_TEXT_INSET: f32 = 8.0;

/// Hairline weight for the frames the page controls draw.
const CONTROL_STROKE: f32 = 1.5;

/// Width the host will advance drawing `text` at `size`, estimated.
fn text_advance(text: &str, size: f32) -> f32 {
    let size = if size > 0.0 { size } else { HOST_BODY_TEXT_SIZE };
    text.chars().count() as f32 * size * AVERAGE_GLYPH_ADVANCE
}

/// Top of a single line of `size` text centred inside a `height`-tall box.
fn centred_line_top(y: f32, height: f32, size: f32) -> f32 {
    let size = if size > 0.0 { size } else { HOST_BODY_TEXT_SIZE };
    y + (height - size * LINE_HEIGHT) / 2.0
}

/// Builds one frame of a plugin-drawn page (spec 32.3).
///
/// A thin constructor over [`PageNode`], never a layout engine: it fills in
/// the fields a control needs to be drawable, hit-testable and announceable at
/// coordinates the author chose, so an author writes a page without hand-
/// filling sixteen struct fields per node and without any chance of shipping a
/// button no screen reader can name.
#[must_use = "call PageBuilder::build to create the frame"]
#[derive(Debug, Clone)]
pub struct PageBuilder {
    generation: u64,
    title: String,
    nodes: Vec<PageNode>,
    focus_node: u32,
    redraw_after_ms: u32,
    close: bool,
    palette: PagePalette,
}

/// Where a control sits, in the page's own logical pixels.
///
/// Controls take one of these rather than four loose floats: the four always
/// travel together, and an argument list long enough to transpose two of them
/// is a bug the compiler cannot catch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PageRect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }
}

/// The RGBA8 buffer [`PageBuilder::canvas`] hands a plugin to draw into.
///
/// Addressed in pixels, in the raster's own coordinates, so the author writes
/// what they mean and never the stride arithmetic behind it. Writes outside
/// the buffer are clipped rather than fatal: a bar chart's last column is
/// computed from plugin state, and a rounding error there should cost a
/// pixel, not the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCanvas {
    pixel_width: u32,
    pixel_height: u32,
    rgba: Vec<u8>,
}

impl PageCanvas {
    pub const fn width(&self) -> u32 {
        self.pixel_width
    }

    pub const fn height(&self) -> u32 {
        self.pixel_height
    }

    /// Paints every pixel, replacing what is there.
    pub fn fill(&mut self, colour: PageColor) {
        let bytes = [colour.r, colour.g, colour.b, colour.a];
        for pixel in self.rgba.chunks_exact_mut(4) {
            pixel.copy_from_slice(&bytes);
        }
    }

    /// Paints one pixel, replacing what is there.
    pub fn set_pixel(&mut self, x: u32, y: u32, colour: PageColor) {
        if x >= self.pixel_width || y >= self.pixel_height {
            return;
        }
        let offset = ((y as usize) * (self.pixel_width as usize) + x as usize) * 4;
        self.rgba[offset..offset + 4].copy_from_slice(&[colour.r, colour.g, colour.b, colour.a]);
    }

    /// Paints an axis-aligned block of pixels, clipped to the buffer.
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, colour: PageColor) {
        let right = x.saturating_add(width).min(self.pixel_width);
        let bottom = y.saturating_add(height).min(self.pixel_height);
        if x >= right || y >= bottom {
            return;
        }
        let bytes = [colour.r, colour.g, colour.b, colour.a];
        let stride = self.pixel_width as usize;
        for row in y..bottom {
            let start = ((row as usize) * stride + x as usize) * 4;
            let end = ((row as usize) * stride + right as usize) * 4;
            for pixel in self.rgba[start..end].chunks_exact_mut(4) {
                pixel.copy_from_slice(&bytes);
            }
        }
    }
}

impl PageBuilder {
    /// Starts a frame answering `generation`, drawn in the host's palette.
    ///
    /// The generation comes straight from the request being answered: the host
    /// drops a frame that answers an older one, so a builder that invented its
    /// own counter would blank the page.
    pub fn new(generation: u64, palette: PagePalette) -> Self {
        Self {
            generation,
            title: String::new(),
            nodes: Vec::new(),
            focus_node: 0,
            redraw_after_ms: 0,
            close: false,
            palette,
        }
    }

    /// Sets the title the launcher shows beside the page.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Asks the host to focus one of this frame's nodes.
    pub fn focus(mut self, node_id: u32) -> Self {
        self.focus_node = node_id;
        self
    }

    /// Asks for another frame after `milliseconds` even without input.
    pub fn redraw_after_ms(mut self, milliseconds: u32) -> Self {
        self.redraw_after_ms = milliseconds;
        self
    }

    /// Ends the page from the plugin's side.
    pub fn close(mut self) -> Self {
        self.close = true;
        self
    }

    /// Adds a node the helpers do not cover.
    pub fn node(mut self, node: PageNode) -> Self {
        self.nodes.push(node);
        self
    }

    /// Fills a rectangle.
    pub fn rect(self, x: f32, y: f32, width: f32, height: f32, fill: PageColor) -> Self {
        self.rounded_rect(x, y, width, height, 0.0, fill)
    }

    /// Fills a rectangle with rounded corners.
    pub fn rounded_rect(
        self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        rounding: f32,
        fill: PageColor,
    ) -> Self {
        self.node(PageNode {
            shape: NodeShape::Rect,
            x,
            y,
            width,
            height,
            fill,
            rounding,
            ..PageNode::default()
        })
    }

    /// Draws one line of text with its top-left corner at `x`, `y`. A `size`
    /// of zero takes the host's body size.
    pub fn text(self, x: f32, y: f32, text: impl Into<String>, size: f32, colour: PageColor) -> Self {
        self.node(PageNode {
            shape: NodeShape::Text,
            x,
            y,
            fill: colour,
            text: text.into(),
            text_size: size,
            ..PageNode::default()
        })
    }

    /// Draws a page heading, announced as one.
    ///
    /// Carries a measured rect as well as glyphs because the rect is what the
    /// host reports upwards: a heading with no extent is text a screen reader
    /// cannot place.
    pub fn heading(self, x: f32, y: f32, text: impl Into<String>) -> Self {
        let text = text.into();
        let width = text_advance(&text, HEADING_TEXT_SIZE);
        let fill = self.palette.text;
        self.node(PageNode {
            shape: NodeShape::Text,
            x,
            y,
            width,
            height: HEADING_TEXT_SIZE * LINE_HEIGHT,
            fill,
            text,
            text_size: HEADING_TEXT_SIZE,
            role: NodeRole::Heading,
            ..PageNode::default()
        })
    }

    /// Draws a button: the accent-filled rect that carries the hit target,
    /// the role and the accessible name, plus its centred caption.
    ///
    /// The caption is a separate anonymous node because only one node may own
    /// `node_id`, and the one that owns it must be the one the host hit-tests.
    pub fn button(self, node_id: u32, rect: PageRect, label: impl Into<String>) -> Self {
        let PageRect { x, y, width, height } = rect;
        let label = label.into();
        let caption = label.clone();
        let accent = self.palette.accent;
        let surface = self.palette.surface;
        let caption_x = x + (width - text_advance(&caption, 0.0)) / 2.0;
        let caption_y = centred_line_top(y, height, 0.0);
        self.node(PageNode {
            shape: NodeShape::Rect,
            x,
            y,
            width,
            height,
            fill: accent,
            rounding: CONTROL_ROUNDING,
            role: NodeRole::Button,
            label,
            node_id,
            ..PageNode::default()
        })
        .text(caption_x, caption_y, caption, 0.0, surface)
    }

    /// Draws a checkbox row: the indicator carries the state and the
    /// semantics, the text beside it is decoration.
    pub fn checkbox(self, node_id: u32, x: f32, y: f32, label: impl Into<String>, checked: bool) -> Self {
        let label = label.into();
        let caption = label.clone();
        let accent = self.palette.accent;
        let muted = self.palette.muted;
        let surface = self.palette.surface;
        let text_colour = self.palette.text;
        let builder = self.node(PageNode {
            shape: NodeShape::Rect,
            x,
            y,
            width: CHECKBOX_EDGE,
            height: CHECKBOX_EDGE,
            fill: if checked { accent } else { PageColor::TRANSPARENT },
            stroke: muted,
            stroke_width: CONTROL_STROKE,
            rounding: CONTROL_ROUNDING / 2.0,
            role: NodeRole::Checkbox,
            label,
            node_id,
            checked,
            ..PageNode::default()
        });
        let builder = if checked {
            // The tick is drawn as text rather than a shape because the
            // vocabulary has no polyline, and it is anonymous so the
            // indicator it sits on keeps the single hit target.
            builder.text(
                x + CHECKBOX_EDGE / 4.0,
                centred_line_top(y, CHECKBOX_EDGE, 0.0),
                "\u{2713}",
                0.0,
                surface,
            )
        } else {
            builder
        };
        builder.text(
            x + CHECKBOX_EDGE + CHECKBOX_LABEL_GAP,
            centred_line_top(y, CHECKBOX_EDGE, 0.0),
            caption,
            0.0,
            text_colour,
        )
    }

    /// Draws a text field showing `value`.
    ///
    /// The page owns the edit state: nothing here tracks a caret or a
    /// selection, because the host routes committed text and named keys and
    /// the plugin decides what they mean.
    pub fn text_field(
        self,
        node_id: u32,
        rect: PageRect,
        label: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        let PageRect { x, y, width, height } = rect;
        let muted = self.palette.muted;
        let text_colour = self.palette.text;
        self.node(PageNode {
            shape: NodeShape::Rect,
            x,
            y,
            width,
            height,
            fill: PageColor::TRANSPARENT,
            stroke: muted,
            stroke_width: CONTROL_STROKE,
            rounding: CONTROL_ROUNDING,
            role: NodeRole::TextField,
            label: label.into(),
            node_id,
            ..PageNode::default()
        })
        .text(
            x + FIELD_TEXT_INSET,
            centred_line_top(y, height, 0.0),
            value,
            0.0,
            text_colour,
        )
    }

    /// Draws a raster the plugin already holds as finished pixels.
    ///
    /// `rgba` is raw RGBA8 - row-major, non-premultiplied, no padding - and
    /// nothing else: the host parses no image format, so a plugin shipping a
    /// PNG decodes it in its own process. The raster is scaled into `rect`,
    /// which is why the pixel dimensions are given separately.
    ///
    /// An empty `label` leaves the node decoration, exactly as an unlabelled
    /// rect is: a picture says nothing to a screen reader that the author did
    /// not say for it.
    pub fn image(
        self,
        rect: PageRect,
        label: impl Into<String>,
        pixel_width: u32,
        pixel_height: u32,
        rgba: impl Into<Vec<u8>>,
    ) -> Result<Self, PageError> {
        let rgba = rgba.into();
        let expected = self.check_raster(pixel_width, pixel_height)?;
        if rgba.len() != expected {
            return Err(PageError::ImageSizeMismatch {
                index: self.nodes.len(),
                expected,
                actual: rgba.len(),
            });
        }
        Ok(self.raster_node(rect, label.into(), pixel_width, pixel_height, rgba))
    }

    /// Draws a raster the plugin paints itself, through a buffer handed to
    /// `draw`.
    ///
    /// The buffer arrives fully transparent and correctly sized, and
    /// [`PageCanvas`] addresses it by pixel, so an author draws a chart
    /// without writing `(y * width + x) * 4` once. It is the same node kind
    /// [`PageBuilder::image`] produces - the host cannot tell a painted
    /// surface from a shipped picture, and has no reason to.
    pub fn canvas(
        self,
        rect: PageRect,
        label: impl Into<String>,
        pixel_width: u32,
        pixel_height: u32,
        draw: impl FnOnce(&mut PageCanvas),
    ) -> Result<Self, PageError> {
        let expected = self.check_raster(pixel_width, pixel_height)?;
        let mut canvas = PageCanvas {
            pixel_width,
            pixel_height,
            rgba: vec![0; expected],
        };
        draw(&mut canvas);
        Ok(self.raster_node(rect, label.into(), pixel_width, pixel_height, canvas.rgba))
    }

    /// The byte count a raster of these dimensions must carry, or the refusal
    /// the host would answer with.
    ///
    /// Checked before the pixels exist rather than after the frame is sent:
    /// the dimensions decide the allocation, so an author who got them wrong
    /// learns it at the call that was wrong and pays for no megabyte the host
    /// was always going to drop.
    fn check_raster(&self, pixel_width: u32, pixel_height: u32) -> Result<usize, PageError> {
        let index = self.nodes.len();
        if pixel_width == 0
            || pixel_height == 0
            || pixel_width > MAX_PAGE_IMAGE_EDGE
            || pixel_height > MAX_PAGE_IMAGE_EDGE
        {
            return Err(PageError::ImageEdgeOutOfRange {
                index,
                pixel_width,
                pixel_height,
            });
        }
        let expected = (pixel_width as usize) * (pixel_height as usize) * 4;
        if expected > MAX_PAGE_IMAGE_BYTES {
            return Err(PageError::ImageTooLarge {
                index,
                bytes: expected,
            });
        }
        Ok(expected)
    }

    /// Pushes the raster node both raster helpers produce.
    ///
    /// A labelled raster takes [`NodeRole::Label`]: it is announced with the
    /// name the author gave and stays out of the focus ring, because a
    /// picture is not something Tab should stop on.
    fn raster_node(
        self,
        rect: PageRect,
        label: String,
        pixel_width: u32,
        pixel_height: u32,
        rgba: Vec<u8>,
    ) -> Self {
        let PageRect { x, y, width, height } = rect;
        let role = if label.is_empty() {
            NodeRole::None
        } else {
            NodeRole::Label
        };
        self.node(PageNode {
            shape: NodeShape::Image,
            x,
            y,
            width,
            height,
            role,
            label,
            image: Some(PageImage {
                pixel_width,
                pixel_height,
                rgba,
            }),
            ..PageNode::default()
        })
    }

    /// Consumes the builder and produces the frame.
    pub fn build(self) -> PageFrame {
        PageFrame {
            generation: self.generation,
            title: self.title,
            nodes: self.nodes,
            focus_node: self.focus_node,
            redraw_after_ms: self.redraw_after_ms,
            close: self.close,
        }
    }
}
