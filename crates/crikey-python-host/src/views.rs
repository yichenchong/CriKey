//! Context-specific standard-view JSON codec. Endpoint ownership is never decoded.
use crikey_core::preview::{PreviewContent, PreviewImage, PreviewText, PreviewValue, PreviewValueKind};
use crikey_core::result_presentation::{ItemPresentation, Scope, ScopeId, Section, SectionId};
use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
use crikey_core::structured_view::*;
use crikey_core::{ActionId, ItemId, PluginId};
use serde_json::{json, Map, Value};
use std::sync::Arc;

fn string(m: &Map<String, Value>, k: &str) -> Option<String> {
    Some(m.get(k)?.as_str()?.to_owned())
}
fn optional(m: &Map<String, Value>, k: &str) -> Option<Option<String>> {
    match m.get(k) {
        None | Some(Value::Null) => Some(None),
        Some(v) => Some(Some(v.as_str()?.to_owned())),
    }
}
fn array<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a [Value]> {
    m.get(k)?.as_array().map(Vec::as_slice)
}
fn owned(owner: &PluginId, item: String) -> OwnedItem {
    OwnedItem {
        owner: owner.clone(),
        item: ItemId(item),
    }
}
pub(crate) fn context(context: &ViewExecutionContext) -> Value {
    json!({"route":context.route.0,"session":{"logical_view":context.session.logical_view.get(),
        "surface":context.session.surface.get()},"revision":context.revision.get()})
}
pub(crate) fn request(id: u64, request: &ViewRequest, owner: &PluginId) -> Option<Value> {
    request.validate().ok()?;
    if request.state.as_ref().is_some_and(|s| {
        s.selected.as_ref().is_some_and(|t| &t.owner != owner)
            || s.input.as_ref().is_some_and(|t| &t.target.owner != owner)
    }) || request.scroll.as_ref().is_some_and(|s| &s.item.owner != owner)
    {
        return None;
    }
    Some(json!({"id":id,"kind":"structured_view","route":request.route.0,
        "session":{"logical_view":request.session.logical_view.get(),"surface":request.session.surface.get()},
        "mode":match request.mode {ViewRequestMode::Open=>"open",ViewRequestMode::Refresh=>"refresh"},
        "previous_revision":request.previous_revision.map(|n|n.get()),
        "reason":match request.reason {ViewRefreshReason::Explicit=>"explicit",ViewRefreshReason::Query=>"query",
            ViewRefreshReason::Selection=>"selection",ViewRefreshReason::Scope=>"scope"},
        "state":request.state.as_ref().map(|s|json!({"selected_item_id":s.selected.as_ref().map(|v|&v.item.0),
            "scope_id":s.scope.as_ref().map(|v|&v.0),"input_item_id":s.input.as_ref().map(|v|&v.target.item.0),
            "input_action_id":s.input.as_ref().and_then(|v|v.action.as_ref().map(|a|&a.0))})),
        "query":request.query,"anchor":request.scroll.as_ref().map(|a|json!({"item_id":a.item.item.0,"row_offset":a.row_offset}))}))
}
pub(crate) fn navigation(frame: &Map<String, Value>) -> Option<StructuredRouteOutcome> {
    let m = frame.get("navigation")?.as_object()?;
    let route = RouteId(string(m, "route")?);
    let outcome = match m.get("kind")?.as_str()? {
        "push" => StructuredRouteOutcome::Push(route),
        "replace" => StructuredRouteOutcome::Replace(route),
        _ => return None,
    };
    outcome.validate().ok()?;
    Some(outcome)
}
fn menu(values: &[Value], depth: usize) -> Option<Vec<ActionMenuEntry>> {
    if depth > MAX_ACTION_DEPTH || values.len() > MAX_ACTIONS_PER_TARGET {
        return None;
    }
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        let m = value.as_object()?;
        result.push(match m.get("kind")?.as_str()? {
            "action" => ActionMenuEntry::Action(ExecutablePresentation {
                action: optional(m, "action_id")?.map(ActionId),
                schema: match m.get("schema") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(Arc::new(crate::typed::schema(v)?)),
                },
                shortcut: optional(m, "shortcut")?,
                destructive: m.get("destructive").map_or(Some(false), Value::as_bool)?,
            }),
            "submenu" => ActionMenuEntry::Submenu {
                id: string(m, "id")?,
                label: string(m, "label")?,
                description: optional(m, "description")?.unwrap_or_default(),
                icon_reference: optional(m, "icon_reference")?,
                entries: menu(array(m, "entries")?, depth + 1)?,
            },
            _ => return None,
        });
    }
    Some(result)
}
fn collection(owner: &PluginId, m: &Map<String, Value>) -> Option<CollectionView> {
    let mut sections = Vec::new();
    for value in array(m, "sections")? {
        let m = value.as_object()?;
        sections.push(Section {
            id: SectionId(string(m, "id")?),
            label: string(m, "label")?,
        });
    }
    let mut scopes = Vec::new();
    for value in array(m, "scopes")? {
        let m = value.as_object()?;
        scopes.push(Scope {
            id: ScopeId(string(m, "id")?),
            label: string(m, "label")?,
        });
    }
    let entries = array(m, "items")?;
    if entries.len() > MAX_VIEW_ITEMS || sections.len() > 128 || scopes.len() > 128 {
        return None;
    }
    let mut items = Vec::with_capacity(entries.len());
    for value in entries {
        let m = value.as_object()?;
        let mut scopes = Vec::new();
        for value in array(m, "scopes")? {
            scopes.push(ScopeId(value.as_str()?.to_owned()));
        }
        items.push(ItemPresentation {
            target: owned(owner, string(m, "item_id")?),
            section: optional(m, "section")?.map(SectionId),
            scopes,
            image: match m.get("image") {
                None | Some(Value::Null) => None,
                Some(value) => Some(Arc::new(image(value)?)),
            },
        });
    }
    Some(CollectionView {
        items: items.into(),
        sections,
        scopes,
        initial_selection: optional(m, "initial_selection")?.map(|s| owned(owner, s)),
        initial_scope: optional(m, "initial_scope")?.map(ScopeId),
    })
}
fn image(value: &Value) -> Option<PreviewImage> {
    let m = value.as_object()?;
    let width = u32::try_from(m.get("width")?.as_u64()?).ok()?;
    let height = u32::try_from(m.get("height")?.as_u64()?).ok()?;
    let values = array(m, "rgba")?;
    let length = usize::try_from(width.checked_mul(height)?.checked_mul(4)?).ok()?;
    if width == 0
        || height == 0
        || width > 1024
        || height > 1024
        || length > 1024 * 1024
        || values.len() != length
    {
        return None;
    }
    let mut rgba = Vec::with_capacity(length);
    for value in values {
        rgba.push(u8::try_from(value.as_u64()?).ok()?);
    }
    Some(PreviewImage { width, height, rgba })
}
fn preview(value: &Value) -> Option<PreviewContent> {
    let m = value.as_object()?;
    let text = optional(m, "text")?.map(|text| {
        if m.get("markdown").and_then(Value::as_bool) == Some(true) {
            PreviewText::Markdown(text)
        } else {
            PreviewText::Plain(text)
        }
    });
    let image = match m.get("image") {
        None | Some(Value::Null) => None,
        Some(value) => Some(image(value)?),
    };
    let mut values = Vec::new();
    for value in array(m, "values")? {
        let m = value.as_object()?;
        values.push(PreviewValue {
            label: string(m, "label")?,
            value: string(m, "value")?,
            kind: match m.get("kind")?.as_str()? {
                "text" => PreviewValueKind::Text,
                "link" => PreviewValueKind::Link,
                "tag" => PreviewValueKind::Tag,
                _ => return None,
            },
        });
    }
    let content = PreviewContent { text, image, values };
    content.validate().ok()?;
    Some(content)
}
pub(crate) enum DescriptorDecode {
    Content(ViewDescriptor),
    Rejected,
}
pub(crate) fn descriptor(owner: &PluginId, value: &Value) -> Option<DescriptorDecode> {
    let m = value.as_object()?;
    let entries = array(m, "definitions")?;
    if entries.len() > MAX_VIEW_ITEMS {
        return None;
    }
    let mut definitions = Vec::with_capacity(entries.len());
    for value in entries {
        let m = value.as_object()?;
        let mut action_sections = Vec::new();
        for section in array(m, "action_sections")? {
            let m = section.as_object()?;
            action_sections.push(ActionSection {
                id: string(m, "id")?,
                label: string(m, "label")?,
                entries: menu(array(m, "entries")?, 1)?,
            });
        }
        definitions.push(RouteItemDefinition {
            item: crate::protocol::decode_item(owner, m.get("item")?)?,
            actionless_policy: match optional(m, "actionless_policy")? {
                None => None,
                Some(p) if p == "plugin" => Some(crikey_core::ExecutionPolicy::Plugin),
                _ => return None,
            },
            primary_action: optional(m, "primary_action")?.map(ActionId),
            action_sections,
        });
    }
    let v = m.get("view")?.as_object()?;
    let view = match v.get("kind")?.as_str()? {
        "list" => StandardView::List(collection(owner, v)?),
        "grid" => StandardView::Grid(GridView {
            collection: collection(owner, v)?,
            columns: u8::try_from(v.get("columns")?.as_u64()?).ok()?,
        }),
        "detail" => {
            let content = preview(v.get("content")?)?;
            let target = optional(v, "target_item_id")?.map(|s| owned(owner, s));
            match DetailView::prepare(content, target) {
                Ok(detail) => StandardView::Detail(detail),
                // Valid public SDK values can fail the host's safe markdown
                // preparation. Refuse content, without killing its producer.
                Err(_) => return Some(DescriptorDecode::Rejected),
            }
        }
        "form" => StandardView::Form(FormView {
            submit: ExecutableKey {
                target: owned(owner, string(v, "item_id")?),
                action: optional(v, "action_id")?.map(ActionId),
            },
        }),
        _ => return None,
    };
    Some(DescriptorDecode::Content(ViewDescriptor {
        title: string(m, "title")?,
        definitions: definitions.into(),
        view,
    }))
}

pub(crate) fn same_item(a: &crikey_core::Item, b: &crikey_core::Item) -> bool {
    a.plugin_id == b.plugin_id
        && a.stable_id == b.stable_id
        && a.category == b.category
        && a.label == b.label
        && a.description == b.description
        && a.target == b.target
        && a.search_terms == b.search_terms
        && a.icon_reference == b.icon_reference
        && a.argument_policy == b.argument_policy
        && a.hit_policy == b.hit_policy
        && a.score_hint == b.score_hint
        && a.metadata == b.metadata
        && a.actions.len() == b.actions.len()
        && a.actions.iter().zip(&b.actions).all(|(a, b)| {
            a.action_id == b.action_id
                && a.label == b.label
                && a.description == b.description
                && a.icon_reference == b.icon_reference
                && a.applicable_categories == b.applicable_categories
                && a.execution_policy == b.execution_policy
        })
}
