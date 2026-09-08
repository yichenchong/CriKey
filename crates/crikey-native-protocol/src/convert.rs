//! Conversion between core catalog values and the native wire schema.

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, NodeRole,
    NodeShape, PageColor, PageFrame, PageImage, PageInput, PageInputKind, PageNode, PluginId,
};

use crate::message;
use crate::wire::UnknownFields;

/// Converts a core item to its lossless proto representation (spec 10.1-10.4).
pub fn to_proto_item(item: &Item) -> message::Item {
    message::Item {
        stable_id: item.stable_id.0.clone(),
        label: item.label.clone(),
        description: item.description.clone(),
        target: item.target.clone(),
        category: category_tag(&item.category),
        search_terms: item.search_terms.clone(),
        icon_reference: item.icon_reference.clone().unwrap_or_default(),
        score_hint: item.score_hint,
        metadata: item.metadata.clone(),
        actions: item.actions.iter().map(to_proto_action).collect(),
        argument_policy: argument_policy_tag(item.argument_policy).to_owned(),
        hit_policy: hit_policy_tag(item.hit_policy).to_owned(),
        unknown: UnknownFields::default(),
    }
}

/// Converts a plugin-owned proto item. The host supplies ownership and derives
/// an id when the plugin leaves `stable_id` empty (spec 10.2).
pub fn from_proto_item(plugin: &PluginId, item: &message::Item) -> Item {
    let category = category_from_tag(&item.category);
    let stable_id = if item.stable_id.is_empty() {
        ItemId::derived(plugin, &category, &item.target)
    } else {
        ItemId(item.stable_id.clone())
    };
    Item {
        stable_id,
        plugin_id: plugin.clone(),
        category,
        label: item.label.clone(),
        description: item.description.clone(),
        target: item.target.clone(),
        search_terms: item.search_terms.clone(),
        icon_reference: (!item.icon_reference.is_empty()).then(|| item.icon_reference.clone()),
        argument_policy: argument_policy_from_tag(&item.argument_policy),
        hit_policy: hit_policy_from_tag(&item.hit_policy),
        score_hint: item.score_hint,
        metadata: item.metadata.clone(),
        actions: item.actions.iter().map(from_proto_action).collect(),
    }
}

/// Wire spelling of an argument policy (spec 10.1).
pub fn argument_policy_tag(policy: ArgumentPolicy) -> &'static str {
    match policy {
        ArgumentPolicy::Forbidden => "forbidden",
        ArgumentPolicy::Optional => "optional",
        ArgumentPolicy::Required => "required",
    }
}

/// Parses an argument policy; an unknown value is the conservative default.
pub fn argument_policy_from_tag(tag: &str) -> ArgumentPolicy {
    match tag {
        "optional" => ArgumentPolicy::Optional,
        "required" => ArgumentPolicy::Required,
        _ => ArgumentPolicy::Forbidden,
    }
}

/// Wire spelling of a hit policy (spec 10.1).
pub fn hit_policy_tag(policy: HitPolicy) -> &'static str {
    match policy {
        HitPolicy::Recorded => "recorded",
        HitPolicy::Ignored => "ignored",
    }
}

/// Parses a hit policy; an unknown value is the conservative default.
pub fn hit_policy_from_tag(tag: &str) -> HitPolicy {
    match tag {
        "ignored" => HitPolicy::Ignored,
        _ => HitPolicy::Recorded,
    }
}

/// Converts a core action to its wire representation.
pub fn to_proto_action(action: &Action) -> message::Action {
    message::Action {
        action_id: action.action_id.0.clone(),
        label: action.label.clone(),
        description: action.description.clone(),
        icon_reference: action.icon_reference.clone().unwrap_or_default(),
        execution_policy: match action.execution_policy {
            ExecutionPolicy::HostMediated => "host-mediated".to_owned(),
            ExecutionPolicy::Plugin => "plugin".to_owned(),
        },
        applicable_categories: action.applicable_categories.iter().map(category_tag).collect(),
        unknown: UnknownFields::default(),
    }
}

/// Converts a plugin action; unknown execution policies are plugin mediated.
pub fn from_proto_action(action: &message::Action) -> Action {
    Action {
        action_id: ActionId(action.action_id.clone()),
        label: action.label.clone(),
        description: action.description.clone(),
        applicable_categories: action
            .applicable_categories
            .iter()
            .map(|tag| category_from_tag(tag))
            .collect(),
        icon_reference: (!action.icon_reference.is_empty()).then(|| action.icon_reference.clone()),
        execution_policy: if action.execution_policy == "host-mediated" {
            ExecutionPolicy::HostMediated
        } else {
            ExecutionPolicy::Plugin
        },
    }
}

/// Re-exported so a transport-level caller does not have to reach into
/// `crikey-core` for the discriminator (spec 10.3).
pub use crikey_core::PLUGIN_DEFINED_PREFIX;

/// Stable, injective category tag used by the proto schema (spec 10.3).
///
/// Delegates to [`Category::wire_tag`]: the encoding lives beside the type it
/// encodes, so this transport and the Python worker protocol cannot drift into
/// two different spellings.
pub fn category_tag(category: &Category) -> String {
    category.wire_tag()
}

/// Parses a category tag, retaining unknown plugin-defined categories.
pub fn category_from_tag(tag: &str) -> Category {
    Category::from_wire_tag(tag)
}

// ---------------------------------------------------------------------------
// Plugin-drawn pages (spec 27)
// ---------------------------------------------------------------------------

/// Converts a decoded page frame to the host's own model.
///
/// Deliberately total: every wire value maps to something, including the
/// values a host must refuse. Rejection is
/// [`crikey_core::PageFrame::validate`]'s job and happens once, on the whole
/// frame, rather than being scattered through the field mapping where a
/// missed case would silently clamp instead of refusing.
pub fn from_proto_page_frame(frame: &message::PageFrame) -> PageFrame {
    PageFrame {
        generation: frame.generation,
        title: frame.title.clone(),
        nodes: frame.nodes.iter().map(from_proto_page_node).collect(),
        focus_node: frame.focus_node,
        redraw_after_ms: frame.redraw_after_ms,
        close: frame.close,
    }
}

/// Converts one wire node, taking only what this host can act on.
///
/// The raster is kept only for the shape that draws one. A newer plugin may
/// send a shape this host has never heard of *and* attach a raster to it, and
/// that node decodes to [`NodeShape::None`] here; carrying the pixels along
/// would leave the frame holding a raster on a shape that cannot draw it,
/// which [`PageFrame::validate`] refuses — turning one unknown shape into a
/// refused page and breaking the forward-compatibility rule of spec 32.5.
///
/// The test is the *wire* code rather than the mapped shape, which keeps the
/// diagnostic that matters. Every code this schema knows carries its raster
/// through, so a plugin attaching one to a rectangle still meets
/// `ImageShapeMismatch` and is told what it did (spec 32.7). Only the
/// unspecified bucket — where every unrecognised code lands, indistinguishably
/// from an explicit zero — gives its raster up, and a node that paints nothing
/// had no use for pixels anyway.
fn from_proto_page_node(node: &message::PageNode) -> PageNode {
    let shape = match node.shape {
        message::PageShapeCode::Rect => NodeShape::Rect,
        message::PageShapeCode::Text => NodeShape::Text,
        message::PageShapeCode::Line => NodeShape::Line,
        message::PageShapeCode::Circle => NodeShape::Circle,
        message::PageShapeCode::Image => NodeShape::Image,
        message::PageShapeCode::ShapeUnspecified => NodeShape::None,
    };
    PageNode {
        shape,
        x: node.x,
        y: node.y,
        width: node.width,
        height: node.height,
        fill: PageColor::from_u32(node.fill),
        stroke: PageColor::from_u32(node.stroke),
        stroke_width: node.stroke_width,
        rounding: node.rounding,
        text: node.text.clone(),
        text_size: node.text_size,
        role: match node.role {
            message::PageRoleCode::Button => NodeRole::Button,
            message::PageRoleCode::Label => NodeRole::Label,
            message::PageRoleCode::Heading => NodeRole::Heading,
            message::PageRoleCode::TextField => NodeRole::TextField,
            message::PageRoleCode::Checkbox => NodeRole::Checkbox,
            message::PageRoleCode::RoleUnspecified => NodeRole::None,
        },
        label: node.label.clone(),
        node_id: node.node_id,
        focus_order: node.focus_order,
        checked: node.checked,
        image: node
            .image
            .as_ref()
            .filter(|_| node.shape != message::PageShapeCode::ShapeUnspecified)
            .map(|image| PageImage {
                pixel_width: image.pixel_width,
                pixel_height: image.pixel_height,
                rgba: image.rgba.clone(),
            }),
    }
}

/// Converts a host input event to the wire form sent to the plugin.
pub fn to_proto_page_input(input: &PageInput) -> message::PageInput {
    message::PageInput {
        kind: match input.kind {
            PageInputKind::Opened => message::PageInputCode::Opened,
            PageInputKind::PointerMoved => message::PageInputCode::PointerMoved,
            PageInputKind::PointerPressed => message::PageInputCode::PointerPressed,
            PageInputKind::PointerReleased => message::PageInputCode::PointerReleased,
            PageInputKind::KeyPressed => message::PageInputCode::KeyPressed,
            PageInputKind::TextInput => message::PageInputCode::TextInput,
            PageInputKind::Activated => message::PageInputCode::Activated,
            PageInputKind::FocusChanged => message::PageInputCode::FocusChanged,
            PageInputKind::Closed => message::PageInputCode::Closed,
            PageInputKind::Unspecified => message::PageInputCode::KindUnspecified,
        },
        x: input.x,
        y: input.y,
        key: input.key.clone(),
        text: input.text.clone(),
        node_id: input.node_id,
        ctrl: input.ctrl,
        shift: input.shift,
        alt: input.alt,
        unknown: UnknownFields::default(),
    }
}

/// Converts an input event received by a plugin back to the core model, which
/// is what an SDK hands its author.
pub fn from_proto_page_input(input: &message::PageInput) -> PageInput {
    PageInput {
        kind: match input.kind {
            message::PageInputCode::Opened => PageInputKind::Opened,
            message::PageInputCode::PointerMoved => PageInputKind::PointerMoved,
            message::PageInputCode::PointerPressed => PageInputKind::PointerPressed,
            message::PageInputCode::PointerReleased => PageInputKind::PointerReleased,
            message::PageInputCode::KeyPressed => PageInputKind::KeyPressed,
            message::PageInputCode::TextInput => PageInputKind::TextInput,
            message::PageInputCode::Activated => PageInputKind::Activated,
            message::PageInputCode::FocusChanged => PageInputKind::FocusChanged,
            message::PageInputCode::Closed => PageInputKind::Closed,
            message::PageInputCode::KindUnspecified => PageInputKind::Unspecified,
        },
        x: input.x,
        y: input.y,
        key: input.key.clone(),
        text: input.text.clone(),
        node_id: input.node_id,
        ctrl: input.ctrl,
        shift: input.shift,
        alt: input.alt,
    }
}

/// Converts a page frame produced by a plugin to the wire form. Used by the
/// SDK rather than the host.
pub fn to_proto_page_frame(frame: &PageFrame) -> message::PageFrame {
    message::PageFrame {
        generation: frame.generation,
        title: frame.title.clone(),
        nodes: frame.nodes.iter().map(to_proto_page_node).collect(),
        focus_node: frame.focus_node,
        redraw_after_ms: frame.redraw_after_ms,
        close: frame.close,
        unknown: UnknownFields::default(),
    }
}

fn to_proto_page_node(node: &PageNode) -> message::PageNode {
    message::PageNode {
        shape: match node.shape {
            NodeShape::Rect => message::PageShapeCode::Rect,
            NodeShape::Text => message::PageShapeCode::Text,
            NodeShape::Line => message::PageShapeCode::Line,
            NodeShape::Circle => message::PageShapeCode::Circle,
            NodeShape::Image => message::PageShapeCode::Image,
            NodeShape::None => message::PageShapeCode::ShapeUnspecified,
        },
        x: node.x,
        y: node.y,
        width: node.width,
        height: node.height,
        fill: node.fill.to_u32(),
        stroke: node.stroke.to_u32(),
        stroke_width: node.stroke_width,
        rounding: node.rounding,
        text: node.text.clone(),
        text_size: node.text_size,
        role: match node.role {
            NodeRole::Button => message::PageRoleCode::Button,
            NodeRole::Label => message::PageRoleCode::Label,
            NodeRole::Heading => message::PageRoleCode::Heading,
            NodeRole::TextField => message::PageRoleCode::TextField,
            NodeRole::Checkbox => message::PageRoleCode::Checkbox,
            NodeRole::None => message::PageRoleCode::RoleUnspecified,
        },
        label: node.label.clone(),
        node_id: node.node_id,
        focus_order: node.focus_order,
        checked: node.checked,
        image: node.image.as_ref().map(|image| message::PageImage {
            pixel_width: image.pixel_width,
            pixel_height: image.pixel_height,
            rgba: image.rgba.clone(),
            unknown: UnknownFields::default(),
        }),
        unknown: UnknownFields::default(),
    }
}
