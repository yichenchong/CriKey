//! Plugin-drawn pages: the retained display list a plugin paints inside the
//! launcher window, and the input the host routes back to it (spec 32).
//!
//! # What crosses the boundary
//!
//! Drawing commands and semantics, never code. A page is a flat list of
//! [`PageNode`]s that the host draws with its own renderer, so the plugin
//! decides *what* appears while the host keeps the frame budget, the palette,
//! the DPI scaling and the process boundary. A plugin that could hand over
//! code would end the invariant that no third-party code runs in the main
//! process.
//!
//! Pixels cross only as a [`PageImage`] on a single node, bounded by
//! [`MAX_PAGE_IMAGE_BYTES`] and [`MAX_PAGE_IMAGE_TOTAL_BYTES`]. Handing over
//! the whole surface instead would cost 1.10 MiB per 720x400 repaint, which is
//! the case those caps exist to keep out.
//!
//! # What a page is not
//!
//! There is no document model here, and that is a real cost to plugin
//! authors, not a detail. Nothing lays out, wraps, reflows or scrolls on the
//! plugin's behalf: a page positions every node itself in logical pixels, runs
//! its own edit state for anything text-like, and rebuilds the list whenever
//! its own state changes. The vocabulary is deliberately closed - a host that
//! accepted open-ended nodes could not promise to draw them.
//!
//! # Accessibility is carried, not inferred
//!
//! A painted glyph is not an accessible name. Assistive technology reads
//! widget semantics, so [`PageNode::role`], [`PageNode::label`] and
//! [`PageNode::focus_order`] travel *beside* the drawing and are the only
//! thing the host reports upwards. A plugin that paints text and leaves the
//! role unset has drawn something no screen reader can describe, which is why
//! [`PageFrame::unlabelled_interactive`] reports it rather than leaving the
//! omission silent.

use std::collections::BTreeSet;

/// The largest display list the host will draw. A page is redrawn on input,
/// so the cost is per keystroke rather than one-off; the cap is what stops a
/// plugin turning a keypress into an unbounded layout pass.
pub const MAX_PAGE_NODES: usize = 4_096;

/// The longest string a single node may carry. Long enough for a paragraph,
/// short enough that the host's text shaping stays bounded per node.
pub const MAX_NODE_TEXT_BYTES: usize = 8_192;

/// The soonest a self-scheduled redraw is honoured, in milliseconds.
///
/// A page asks for its next frame with `redraw_after_ms`, and nothing else
/// bounds how often it may ask. That is affordable for a display list of
/// geometry and it is not affordable once a frame may carry
/// [`MAX_PAGE_IMAGE_TOTAL_BYTES`] of raster: at a one millisecond period those
/// are gigabytes a second of encode, transport and decode, which is the cost
/// profile ADR-0017 and ADR-0020 both refused. Sixteen milliseconds is one
/// frame at the 60 Hz baseline the launcher's own budgets are written against
/// (spec 25.1). It is a floor on the page's own timer, not a claim about what
/// the display can present: a page that needs to track something faster
/// should redraw from input, which this does not clamp.
///
/// This bounds self-scheduled redraws only. A frame answering user input is
/// not delayed: input already arrives no faster than a person generates it.
pub const MIN_PAGE_REDRAW_MS: u32 = 16;

/// The largest raster a single node may carry. Exactly a 512x512 RGBA
/// surface, which is the size a launcher page has any business showing: a
/// preview, a swatch, a chart, an icon at any sane density.
pub const MAX_PAGE_IMAGE_BYTES: usize = 1024 * 1024;

/// The largest raster a whole frame may carry, summed across its nodes. Half
/// of `MAX_FRAME_BYTES`, so a page full of rasters still leaves room for the
/// rest of the display list inside one protocol frame.
pub const MAX_PAGE_IMAGE_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// The longest side of a raster, in pixels. Bounds the texture the host
/// uploads independently of the byte count, so a 1x262144 strip is refused
/// on its shape rather than sneaking under the byte cap.
pub const MAX_PAGE_IMAGE_EDGE: u32 = 1024;

/// The largest page the host will ask for, in logical pixels. Bounds the
/// coordinate space a plugin can place nodes in, so a stray offset lands
/// outside the clip rather than in floating-point territory.
pub const MAX_PAGE_EDGE: f32 = 4_096.0;

/// What a node paints. Closed by design: the host must be able to draw every
/// variant with its own renderer, so authors cannot add one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeShape {
    /// Nothing is painted. Useful for a node that exists only to be a
    /// focusable, labelled hit target over other drawing.
    #[default]
    None,
    /// A filled and optionally stroked rectangle, rounded by
    /// [`PageNode::rounding`].
    Rect,
    /// A single run of text, drawn at the node's top-left corner in the
    /// node's fill colour. The host does not wrap it.
    Text,
    /// A straight line from the node's top-left to its bottom-right corner.
    Line,
    /// A circle inscribed in the node's rectangle.
    Circle,
    /// A raster drawn into the node's rectangle, carried by
    /// [`PageNode::image`]. This is the one node whose content the host does
    /// not compose: the plugin supplies finished pixels, so it covers both a
    /// picture the plugin shipped and a surface the plugin drew itself.
    ///
    /// A raster carries no meaning. `role` and `label` are the only things
    /// that make it more than decoration, exactly as for every other node.
    Image,
}

/// Finished pixels for a [`NodeShape::Image`] node.
///
/// Raw RGBA8 and nothing else. The host parses no image format: a decoder is
/// an attack surface, and a decompression bomb is a byte count that says
/// nothing about the allocation behind it. With raw pixels the wire length is
/// the memory cost, already bounded by `MAX_FRAME_BYTES` and the decoder's
/// allocation budget before this type exists; the dimensions are then
/// cross-checked against that length by [`PageFrame::validate`], which runs
/// after decoding rather than before it. A plugin holding a PNG decodes it in
/// its own process, where a malformed file costs its author a worker and
/// costs the launcher nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageImage {
    /// Width in pixels of the raster, not of the node it is drawn into. The
    /// host scales to the node's rectangle.
    pub pixel_width: u32,
    /// Height in pixels of the raster.
    pub pixel_height: u32,
    /// Exactly `pixel_width * pixel_height * 4` bytes, row-major, no padding,
    /// non-premultiplied.
    pub rgba: Vec<u8>,
}

impl PageImage {
    /// The byte count this raster must carry to match its own dimensions.
    ///
    /// Returns `None` when the product overflows, which is itself a refusal:
    /// a plugin describing a raster that cannot exist is not owed an
    /// allocation attempt.
    pub fn expected_bytes(&self) -> Option<usize> {
        (self.pixel_width as usize)
            .checked_mul(self.pixel_height as usize)?
            .checked_mul(4)
    }
}

/// What a node *is*, as opposed to what it looks like. This is the entire
/// accessibility contract: the host reports these to the platform, and a node
/// left [`NodeRole::None`] is decoration as far as assistive technology is
/// concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeRole {
    /// Decoration. Not reported, not focusable, not hit-tested.
    #[default]
    None,
    /// Activated by click, Enter or Space; the plugin receives
    /// [`PageInputKind::Activated`].
    Button,
    /// Static text whose accessible name is [`PageNode::label`].
    Label,
    /// A heading, reported as structure so a page can be navigated by section.
    Heading,
    /// An editable field. While focused, typed text arrives as
    /// [`PageInputKind::TextInput`] instead of reaching the query field.
    TextField,
    /// A two-state control whose state is [`PageNode::checked`].
    Checkbox,
}

impl NodeRole {
    /// Whether the host gives this role a hit target and a place in the focus
    /// ring. Labels and headings are announced but never focused, which keeps
    /// Tab moving between the things a user can actually operate.
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Button | Self::TextField | Self::Checkbox)
    }

    /// Whether the role is reported to assistive technology at all.
    pub fn is_announced(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// A straight 8-bit-per-channel colour. Carried as four bytes rather than a
/// theme reference because a plugin draws its own surface; the host hands it
/// the palette so it can match, but never overrides what it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl PageColor {
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Packs to `0xRRGGBBAA`, the form the wire carries.
    pub const fn to_u32(self) -> u32 {
        ((self.r as u32) << 24) | ((self.g as u32) << 16) | ((self.b as u32) << 8) | self.a as u32
    }

    pub const fn from_u32(value: u32) -> Self {
        Self {
            r: (value >> 24) as u8,
            g: (value >> 16) as u8,
            b: (value >> 8) as u8,
            a: value as u8,
        }
    }

    /// Whether painting this colour could change a single pixel. A fully
    /// transparent fill is not an error - it is how a hit target with no
    /// appearance is spelled - so the renderer skips it rather than refusing.
    pub const fn is_visible(self) -> bool {
        self.a != 0
    }
}

/// The host's surface colours, handed to a plugin with every frame request.
///
/// A page draws its own background and controls, so without these it would
/// have to hard-code colours and would stop matching the launcher the moment
/// the theme changed. Passing them is what lets a page look like part of the
/// application rather than a window pasted into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PagePalette {
    pub surface: PageColor,
    pub text: PageColor,
    pub accent: PageColor,
    pub muted: PageColor,
}

/// One entry in a page's display list.
///
/// Geometry is in logical pixels relative to the page's own top-left corner,
/// so a plugin never learns where its page sits on screen and cannot draw
/// outside it: the host clips.
#[derive(Debug, Clone, PartialEq)]
pub struct PageNode {
    pub shape: NodeShape,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Fill for [`NodeShape::Rect`] and [`NodeShape::Circle`], and the glyph
    /// colour for [`NodeShape::Text`].
    pub fill: PageColor,
    pub stroke: PageColor,
    pub stroke_width: f32,
    /// Corner radius for [`NodeShape::Rect`], clamped to half the shorter
    /// side by the renderer so a rounding larger than the rectangle cannot
    /// invert it.
    pub rounding: f32,
    pub text: String,
    /// Point size for [`NodeShape::Text`]. Zero takes the host's body size,
    /// which is how a page inherits the launcher's typography by default.
    pub text_size: f32,
    pub role: NodeRole,
    /// The accessible name. Falls back to [`PageNode::text`] when empty, so
    /// a labelled button need not repeat itself.
    pub label: String,
    /// Identifies the node across frames. Non-zero on anything the plugin
    /// wants to hear about; input events name the node they landed on, so a
    /// plugin does not hit-test its own geometry. Zero means anonymous.
    pub node_id: u32,
    /// Position in the Tab ring. Ties break by document order, so a plugin
    /// that leaves this zero gets the order it emitted.
    pub focus_order: u32,
    pub checked: bool,
    /// Finished pixels, required by [`NodeShape::Image`] and refused on every
    /// other shape. Carried inline rather than as a path: a reference would
    /// mean the host opening a file a plugin named, which is a filesystem
    /// permission surface bought for no gain when the plugin can already read
    /// its own package.
    pub image: Option<PageImage>,
}

impl Default for PageNode {
    fn default() -> Self {
        Self {
            shape: NodeShape::None,
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            fill: PageColor::TRANSPARENT,
            stroke: PageColor::TRANSPARENT,
            stroke_width: 0.0,
            rounding: 0.0,
            text: String::new(),
            text_size: 0.0,
            role: NodeRole::None,
            label: String::new(),
            node_id: 0,
            focus_order: 0,
            checked: false,
            image: None,
        }
    }
}

impl PageNode {
    /// The name assistive technology should read, or `None` when the node is
    /// pure decoration.
    pub fn accessible_name(&self) -> Option<&str> {
        if !self.role.is_announced() {
            return None;
        }
        let name = if self.label.is_empty() {
            self.text.as_str()
        } else {
            self.label.as_str()
        };
        (!name.is_empty()).then_some(name)
    }

    /// Whether the host should give this node a hit target: it must be
    /// operable *and* addressable, because an interactive node with no id has
    /// nowhere to send the event it would generate.
    pub fn is_focusable(&self) -> bool {
        self.role.is_interactive() && self.node_id != 0
    }
}

/// Why a page frame was refused. A plugin cannot be trusted to bound its own
/// output, so every one of these is a refusal of the frame rather than a
/// clamp: silently drawing two thirds of a page would look like a rendering
/// bug in the launcher instead of a fault in the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageError {
    /// More nodes than [`MAX_PAGE_NODES`].
    TooManyNodes { nodes: usize },
    /// A node's text exceeds [`MAX_NODE_TEXT_BYTES`].
    TextTooLong { index: usize, bytes: usize },
    /// A coordinate was NaN or infinite. Left to reach the renderer these
    /// poison egui's layout arithmetic for the whole frame, including the
    /// launcher's own chrome, so they are refused at the edge.
    NonFiniteGeometry { index: usize },
    /// A coordinate was outside [`MAX_PAGE_EDGE`].
    GeometryOutOfRange { index: usize },
    /// Two nodes claimed the same non-zero id, so an input event naming it
    /// would be ambiguous.
    DuplicateNodeId { node_id: u32 },
    /// A [`NodeShape::Image`] node carried no raster, or a raster arrived on a
    /// node of some other shape. Both are refused rather than ignored: the
    /// first would draw a hole, the second means the plugin and the host
    /// disagree about what the node is.
    ImageShapeMismatch { index: usize },
    /// A raster's byte count does not match its own dimensions.
    ImageSizeMismatch {
        index: usize,
        expected: usize,
        actual: usize,
    },
    /// A raster exceeds [`MAX_PAGE_IMAGE_BYTES`].
    ImageTooLarge { index: usize, bytes: usize },
    /// A raster's width or height exceeds [`MAX_PAGE_IMAGE_EDGE`], or is zero.
    ImageEdgeOutOfRange {
        index: usize,
        pixel_width: u32,
        pixel_height: u32,
    },
    /// The frame's rasters together exceed [`MAX_PAGE_IMAGE_TOTAL_BYTES`].
    ImageTotalTooLarge { bytes: usize },
}

impl std::fmt::Display for PageError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyNodes { nodes } => {
                write!(out, "the page carried {nodes} nodes, more than the {MAX_PAGE_NODES} a frame may draw")
            }
            Self::TextTooLong { index, bytes } => write!(
                out,
                "node {index} carried {bytes} bytes of text, more than the {MAX_NODE_TEXT_BYTES} a node may draw"
            ),
            Self::NonFiniteGeometry { index } => {
                write!(out, "node {index} has a coordinate that is not a finite number")
            }
            Self::GeometryOutOfRange { index } => {
                write!(out, "node {index} lies outside the {MAX_PAGE_EDGE} logical pixel page limit")
            }
            Self::DuplicateNodeId { node_id } => {
                write!(out, "node id {node_id} was claimed twice, so input for it would be ambiguous")
            }
            Self::ImageShapeMismatch { index } => write!(
                out,
                "node {index} must carry a raster if and only if its shape is `Image`"
            ),
            Self::ImageSizeMismatch {
                index,
                expected,
                actual,
            } => write!(
                out,
                "node {index} declares a raster of {expected} bytes and carried {actual}"
            ),
            Self::ImageTooLarge { index, bytes } => write!(
                out,
                "node {index} carried a {bytes} byte raster, more than the {MAX_PAGE_IMAGE_BYTES} a node may draw"
            ),
            Self::ImageEdgeOutOfRange {
                index,
                pixel_width,
                pixel_height,
            } => write!(
                out,
                "node {index} declares a {pixel_width}x{pixel_height} raster, outside the 1 to {MAX_PAGE_IMAGE_EDGE} pixels a side the host will upload"
            ),
            Self::ImageTotalTooLarge { bytes } => write!(
                out,
                "the frame carried {bytes} bytes of raster, more than the {MAX_PAGE_IMAGE_TOTAL_BYTES} one frame may draw"
            ),
        }
    }
}

impl std::error::Error for PageError {}

/// One frame of a plugin-drawn page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageFrame {
    /// The request this answers. The host drops a frame whose generation is
    /// older than the page's current one, so a slow plugin cannot repaint the
    /// user's screen with an answer to a keystroke they have moved past.
    pub generation: u64,
    /// Shown in the launcher's status line, so a user can always see which
    /// plugin owns the surface they are looking at.
    pub title: String,
    pub nodes: Vec<PageNode>,
    /// The node the plugin wants focused, or zero to leave focus alone. A
    /// plugin may move focus on its own frames - that is how a page advances
    /// through a form - but only within its own nodes.
    pub focus_node: u32,
    /// Asks for another frame after this many milliseconds even without
    /// input. Zero means the page is static until the user does something,
    /// which is the case that costs nothing.
    pub redraw_after_ms: u32,
    /// The plugin asking the host to close the page, which is how a page
    /// finishes its own job without the user pressing Escape.
    pub close: bool,
}

impl PageFrame {
    /// Rejects a frame no host should draw. Called on every frame that
    /// arrives from a plugin, before any of it reaches the renderer.
    pub fn validate(&self) -> Result<(), PageError> {
        if self.nodes.len() > MAX_PAGE_NODES {
            return Err(PageError::TooManyNodes {
                nodes: self.nodes.len(),
            });
        }
        let mut claimed = BTreeSet::new();
        let mut raster_bytes: usize = 0;
        for (index, node) in self.nodes.iter().enumerate() {
            if node.text.len() > MAX_NODE_TEXT_BYTES {
                return Err(PageError::TextTooLong {
                    index,
                    bytes: node.text.len(),
                });
            }
            let geometry = [
                node.x,
                node.y,
                node.width,
                node.height,
                node.stroke_width,
                node.rounding,
                node.text_size,
            ];
            if geometry.iter().any(|value| !value.is_finite()) {
                return Err(PageError::NonFiniteGeometry { index });
            }
            if geometry.iter().any(|value| value.abs() > MAX_PAGE_EDGE) {
                return Err(PageError::GeometryOutOfRange { index });
            }
            if node.node_id != 0 && !claimed.insert(node.node_id) {
                return Err(PageError::DuplicateNodeId {
                    node_id: node.node_id,
                });
            }
            // Shape and payload have to agree before the payload is trusted:
            // a raster on a `Rect` means the two sides disagree about what
            // this node is, and disagreement is not something to paper over.
            match (node.shape, node.image.as_ref()) {
                (NodeShape::Image, Some(image)) => {
                    // Dimensions first. They decide the allocation, so they
                    // are checked before any arithmetic that depends on them.
                    if image.pixel_width == 0
                        || image.pixel_height == 0
                        || image.pixel_width > MAX_PAGE_IMAGE_EDGE
                        || image.pixel_height > MAX_PAGE_IMAGE_EDGE
                    {
                        return Err(PageError::ImageEdgeOutOfRange {
                            index,
                            pixel_width: image.pixel_width,
                            pixel_height: image.pixel_height,
                        });
                    }
                    let expected = image.expected_bytes().ok_or(PageError::ImageEdgeOutOfRange {
                        index,
                        pixel_width: image.pixel_width,
                        pixel_height: image.pixel_height,
                    })?;
                    if image.rgba.len() != expected {
                        return Err(PageError::ImageSizeMismatch {
                            index,
                            expected,
                            actual: image.rgba.len(),
                        });
                    }
                    if expected > MAX_PAGE_IMAGE_BYTES {
                        return Err(PageError::ImageTooLarge {
                            index,
                            bytes: expected,
                        });
                    }
                    raster_bytes = raster_bytes.saturating_add(expected);
                    if raster_bytes > MAX_PAGE_IMAGE_TOTAL_BYTES {
                        return Err(PageError::ImageTotalTooLarge { bytes: raster_bytes });
                    }
                }
                // An `Image` with nothing to draw, or a raster on a shape the
                // host composes itself.
                (NodeShape::Image, None) | (_, Some(_)) => {
                    return Err(PageError::ImageShapeMismatch { index })
                }
                (_, None) => {}
            }
        }
        Ok(())
    }

    /// The nodes a user can reach with Tab, in ring order.
    pub fn focus_ring(&self) -> Vec<u32> {
        let mut ring: Vec<(u32, usize, u32)> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.is_focusable())
            .map(|(index, node)| (node.focus_order, index, node.node_id))
            .collect();
        ring.sort_by_key(|(order, index, _)| (*order, *index));
        ring.into_iter().map(|(_, _, node_id)| node_id).collect()
    }

    pub fn node(&self, node_id: u32) -> Option<&PageNode> {
        (node_id != 0).then(|| self.nodes.iter().find(|node| node.node_id == node_id))?
    }

    /// Interactive nodes with nothing for a screen reader to say. Not a
    /// refusal - a page like this draws correctly and works with a mouse and
    /// keyboard - but it is invisible to assistive technology, so the host
    /// reports it rather than letting the omission pass unnoticed.
    pub fn unlabelled_interactive(&self) -> Vec<u32> {
        self.nodes
            .iter()
            .filter(|node| node.is_focusable() && node.accessible_name().is_none())
            .map(|node| node.node_id)
            .collect()
    }
}

/// What happened to a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageInputKind {
    #[default]
    Unspecified,
    /// The page was opened. Always the first event a plugin sees, and the
    /// only one that arrives without the user having done anything.
    Opened,
    PointerMoved,
    PointerPressed,
    PointerReleased,
    /// A key the host did not reserve for itself.
    KeyPressed,
    /// Committed text, already through the platform's input method, so a
    /// plugin never sees a half-composed sequence.
    TextInput,
    /// Enter, Space or a click on a focusable node. Sent in addition to the
    /// raw key or pointer event so a plugin can honour activation without
    /// re-implementing the convention.
    Activated,
    FocusChanged,
    /// The page is going away. Delivered before the host drops the session so
    /// a plugin can release whatever it was holding.
    Closed,
}

/// One input event routed to a page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageInput {
    pub kind: PageInputKind,
    /// Pointer position in the page's own coordinates.
    pub x: f32,
    pub y: f32,
    /// The named key for [`PageInputKind::KeyPressed`], in the host's own
    /// spelling (`Enter`, `ArrowDown`, `A`). Named rather than numeric so the
    /// meaning survives a plugin written against a different SDK version.
    pub key: String,
    pub text: String,
    /// The node the event landed on, or zero when it landed on none. The host
    /// hit-tests, so a plugin does not have to keep its own geometry index.
    pub node_id: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl PageInput {
    pub fn new(kind: PageInputKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(node_id: u32, role: NodeRole) -> PageNode {
        PageNode {
            role,
            node_id,
            label: "ok".to_owned(),
            ..PageNode::default()
        }
    }

    /// A raster node whose bytes genuinely match its dimensions.
    fn raster(pixel_width: u32, pixel_height: u32) -> PageNode {
        PageNode {
            shape: NodeShape::Image,
            image: Some(PageImage {
                pixel_width,
                pixel_height,
                rgba: vec![0; (pixel_width as usize) * (pixel_height as usize) * 4],
            }),
            ..PageNode::default()
        }
    }

    #[test]
    fn a_raster_matching_its_own_dimensions_is_drawn() {
        let frame = PageFrame {
            nodes: vec![raster(4, 4)],
            ..PageFrame::default()
        };
        assert_eq!(frame.validate(), Ok(()));
    }

    /// The byte count is the allocation. A frame claiming a 512x512 surface
    /// and carrying four bytes would have the host size a texture from the
    /// declaration and read from the buffer, which is the shape of an
    /// out-of-bounds read rather than a cosmetic mismatch.
    #[test]
    fn a_raster_that_lies_about_its_length_is_refused() {
        let mut node = raster(8, 8);
        node.image
            .as_mut()
            .expect("the raster is present")
            .rgba
            .truncate(4);
        let frame = PageFrame {
            nodes: vec![node],
            ..PageFrame::default()
        };
        assert_eq!(
            frame.validate(),
            Err(PageError::ImageSizeMismatch {
                index: 0,
                expected: 256,
                actual: 4,
            })
        );
    }

    /// Dimensions are checked before the length arithmetic that depends on
    /// them, so a raster that could never exist is refused on its shape
    /// rather than through an overflowed multiplication.
    #[test]
    fn a_raster_larger_than_the_edge_limit_is_refused_before_its_bytes_are_sized() {
        let frame = PageFrame {
            nodes: vec![PageNode {
                shape: NodeShape::Image,
                image: Some(PageImage {
                    pixel_width: u32::MAX,
                    pixel_height: u32::MAX,
                    rgba: Vec::new(),
                }),
                ..PageNode::default()
            }],
            ..PageFrame::default()
        };
        assert_eq!(
            frame.validate(),
            Err(PageError::ImageEdgeOutOfRange {
                index: 0,
                pixel_width: u32::MAX,
                pixel_height: u32::MAX,
            })
        );
    }

    #[test]
    fn a_zero_sided_raster_is_refused() {
        let frame = PageFrame {
            nodes: vec![PageNode {
                shape: NodeShape::Image,
                image: Some(PageImage {
                    pixel_width: 0,
                    pixel_height: 8,
                    rgba: Vec::new(),
                }),
                ..PageNode::default()
            }],
            ..PageFrame::default()
        };
        assert_eq!(
            frame.validate(),
            Err(PageError::ImageEdgeOutOfRange {
                index: 0,
                pixel_width: 0,
                pixel_height: 8,
            })
        );
    }

    /// One raster inside the per-node cap, many rasters past the per-frame
    /// one. Without the running total a page could hold megabytes of texture
    /// while every individual node looked reasonable.
    #[test]
    fn rasters_that_are_each_legal_can_still_exceed_the_frame_budget() {
        let one = MAX_PAGE_IMAGE_BYTES;
        let nodes = (0..(MAX_PAGE_IMAGE_TOTAL_BYTES / one) + 1)
            .map(|_| raster(512, 512))
            .collect::<Vec<_>>();
        let frame = PageFrame {
            nodes,
            ..PageFrame::default()
        };
        assert_eq!(
            frame.validate(),
            Err(PageError::ImageTotalTooLarge {
                bytes: MAX_PAGE_IMAGE_TOTAL_BYTES + one,
            })
        );
    }

    /// Both sides within the edge limit, the product past the byte cap. The
    /// dimensions are what a reader checks first, so the cap that actually
    /// bounds the upload needs a case where only it can refuse.
    #[test]
    fn a_raster_inside_the_edge_limit_can_still_exceed_the_byte_cap() {
        let frame = PageFrame {
            nodes: vec![raster(MAX_PAGE_IMAGE_EDGE, MAX_PAGE_IMAGE_EDGE / 2)],
            ..PageFrame::default()
        };
        assert_eq!(
            frame.validate(),
            Err(PageError::ImageTooLarge {
                index: 0,
                bytes: MAX_PAGE_IMAGE_BYTES * 2,
            })
        );
    }

    /// Shape and payload disagreeing means the two sides do not agree what
    /// the node is, which is refused in both directions rather than resolved
    /// in the host's favour.
    #[test]
    fn a_raster_on_a_composed_shape_is_refused() {
        let frame = PageFrame {
            nodes: vec![PageNode {
                shape: NodeShape::Rect,
                image: Some(PageImage {
                    pixel_width: 1,
                    pixel_height: 1,
                    rgba: vec![0; 4],
                }),
                ..PageNode::default()
            }],
            ..PageFrame::default()
        };
        assert_eq!(frame.validate(), Err(PageError::ImageShapeMismatch { index: 0 }));
    }

    #[test]
    fn an_image_shape_with_nothing_to_draw_is_refused() {
        let frame = PageFrame {
            nodes: vec![PageNode {
                shape: NodeShape::Image,
                ..PageNode::default()
            }],
            ..PageFrame::default()
        };
        assert_eq!(frame.validate(), Err(PageError::ImageShapeMismatch { index: 0 }));
    }

    #[test]
    fn a_colour_survives_the_wire_form() {
        let colour = PageColor::rgba(18, 52, 86, 255);
        assert_eq!(PageColor::from_u32(colour.to_u32()), colour);
        assert_eq!(colour.to_u32(), 0x1234_56ff);
    }

    #[test]
    fn a_not_a_number_coordinate_is_refused_rather_than_drawn() {
        // egui's layout arithmetic propagates NaN across the whole frame, so
        // a page that smuggled one in would break the launcher's own chrome
        // and not merely its own rectangle.
        let frame = PageFrame {
            nodes: vec![PageNode {
                x: f32::NAN,
                ..PageNode::default()
            }],
            ..PageFrame::default()
        };
        assert_eq!(frame.validate(), Err(PageError::NonFiniteGeometry { index: 0 }));
    }

    #[test]
    fn an_infinite_coordinate_is_refused_too() {
        let frame = PageFrame {
            nodes: vec![PageNode {
                height: f32::INFINITY,
                ..PageNode::default()
            }],
            ..PageFrame::default()
        };
        assert_eq!(frame.validate(), Err(PageError::NonFiniteGeometry { index: 0 }));
    }

    #[test]
    fn a_repeated_node_id_is_refused_because_input_for_it_is_ambiguous() {
        let frame = PageFrame {
            nodes: vec![node(7, NodeRole::Button), node(7, NodeRole::Button)],
            ..PageFrame::default()
        };
        assert_eq!(frame.validate(), Err(PageError::DuplicateNodeId { node_id: 7 }));
    }

    #[test]
    fn anonymous_nodes_may_repeat_because_nothing_is_addressed_to_them() {
        let frame = PageFrame {
            nodes: vec![PageNode::default(), PageNode::default()],
            ..PageFrame::default()
        };
        assert_eq!(frame.validate(), Ok(()));
    }

    #[test]
    fn the_node_cap_is_enforced_on_the_frame_that_exceeds_it() {
        let frame = PageFrame {
            nodes: vec![PageNode::default(); MAX_PAGE_NODES + 1],
            ..PageFrame::default()
        };
        assert_eq!(
            frame.validate(),
            Err(PageError::TooManyNodes {
                nodes: MAX_PAGE_NODES + 1
            })
        );
    }

    #[test]
    fn the_focus_ring_honours_declared_order_then_document_order() {
        let frame = PageFrame {
            nodes: vec![
                PageNode {
                    focus_order: 2,
                    ..node(10, NodeRole::Button)
                },
                PageNode {
                    focus_order: 1,
                    ..node(20, NodeRole::TextField)
                },
                // Same order as the first: document order breaks the tie.
                PageNode {
                    focus_order: 2,
                    ..node(30, NodeRole::Checkbox)
                },
                // Announced but not operable, so never in the ring.
                PageNode {
                    focus_order: 0,
                    ..node(40, NodeRole::Label)
                },
            ],
            ..PageFrame::default()
        };
        assert_eq!(frame.focus_ring(), vec![20, 10, 30]);
    }

    #[test]
    fn an_interactive_node_without_an_id_is_not_focusable() {
        // It could be drawn and even clicked, but the event would have
        // nowhere to go, so it is not offered to the user as reachable.
        let orphan = PageNode {
            role: NodeRole::Button,
            ..PageNode::default()
        };
        assert!(!orphan.is_focusable());
    }

    #[test]
    fn a_button_falls_back_to_its_drawn_text_for_its_accessible_name() {
        let button = PageNode {
            role: NodeRole::Button,
            text: "Retry".to_owned(),
            ..PageNode::default()
        };
        assert_eq!(button.accessible_name(), Some("Retry"));
    }

    #[test]
    fn an_explicit_label_outranks_the_drawn_text() {
        // The drawn glyph is often an icon or an abbreviation; the label is
        // what the plugin wants said out loud.
        let button = PageNode {
            role: NodeRole::Button,
            text: "x".to_owned(),
            label: "Close the preview".to_owned(),
            ..PageNode::default()
        };
        assert_eq!(button.accessible_name(), Some("Close the preview"));
    }

    #[test]
    fn decoration_is_never_announced_however_much_text_it_carries() {
        let decoration = PageNode {
            text: "12 results".to_owned(),
            ..PageNode::default()
        };
        assert_eq!(decoration.accessible_name(), None);
    }

    #[test]
    fn an_unlabelled_button_is_reported_rather_than_passing_silently() {
        let frame = PageFrame {
            nodes: vec![
                PageNode {
                    role: NodeRole::Button,
                    node_id: 5,
                    ..PageNode::default()
                },
                node(6, NodeRole::Button),
            ],
            ..PageFrame::default()
        };
        assert_eq!(frame.validate(), Ok(()));
        assert_eq!(frame.unlabelled_interactive(), vec![5]);
    }
}
