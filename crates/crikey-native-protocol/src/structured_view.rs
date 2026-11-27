//! Bounded standard-view conversion. Endpoint ownership is never guest supplied.
use crate::{message as m, ProtocolError};
use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
use crikey_core::structured_view::*;
use crikey_core::{ActionId, ExecutionPolicy, ItemId, PluginId, PreviewOutcome};
use std::{num::NonZeroU64, sync::Arc};
fn invalid() -> ProtocolError {
    ProtocolError::Malformed("invalid structured view".into())
}
fn nonzero(n: u64) -> Result<NonZeroU64, ProtocolError> {
    NonZeroU64::new(n).ok_or_else(invalid)
}
fn owned(owner: &PluginId, item: String) -> OwnedItem {
    OwnedItem {
        owner: owner.clone(),
        item: ItemId(item),
    }
}
fn executable(owner: &PluginId, v: m::ViewExecutable) -> ExecutableKey {
    ExecutableKey {
        target: owned(owner, v.item_id),
        action: v.action_id.map(ActionId),
    }
}
fn wire_executable(v: &ExecutableKey) -> m::ViewExecutable {
    m::ViewExecutable {
        item_id: v.target.item.0.clone(),
        action_id: v.action.as_ref().map(|a| a.0.clone()),
        ..Default::default()
    }
}
pub fn from_context(v: m::ViewExecutionContext) -> Result<ViewExecutionContext, ProtocolError> {
    let result = ViewExecutionContext {
        route: RouteId(v.route_id),
        session: ViewSessionIdentity {
            logical_view: nonzero(v.logical_view)?,
            surface: nonzero(v.surface)?,
        },
        revision: nonzero(v.revision)?,
    };
    result.validate().map_err(|_| invalid())?;
    Ok(result)
}
pub fn to_context(v: &ViewExecutionContext) -> m::ViewExecutionContext {
    m::ViewExecutionContext {
        route_id: v.route.0.clone(),
        logical_view: v.session.logical_view.get(),
        surface: v.session.surface.get(),
        revision: v.revision.get(),
        ..Default::default()
    }
}
pub fn from_navigation(v: m::RouteNavigation) -> Result<StructuredRouteOutcome, ProtocolError> {
    let route = RouteId(v.route_id);
    let result = match v.kind {
        1 => StructuredRouteOutcome::Push(route),
        2 => StructuredRouteOutcome::Replace(route),
        _ => return Err(invalid()),
    };
    result.validate().map_err(|_| invalid())?;
    Ok(result)
}
pub fn to_navigation(v: &StructuredRouteOutcome) -> Result<m::RouteNavigation, ProtocolError> {
    v.validate().map_err(|_| invalid())?;
    let (kind, route) = match v {
        StructuredRouteOutcome::Push(r) => (1, r),
        StructuredRouteOutcome::Replace(r) => (2, r),
    };
    Ok(m::RouteNavigation {
        kind,
        route_id: route.0.clone(),
        ..Default::default()
    })
}
pub fn from_request(v: m::StructuredViewRequest, owner: &PluginId) -> Result<ViewRequest, ProtocolError> {
    request_charge(&v)?;
    let result = ViewRequest {
        route: RouteId(v.route_id),
        session: ViewSessionIdentity {
            logical_view: nonzero(v.logical_view)?,
            surface: nonzero(v.surface)?,
        },
        mode: match v.mode {
            1 => ViewRequestMode::Open,
            2 => ViewRequestMode::Refresh,
            _ => return Err(invalid()),
        },
        previous_revision: v.previous_revision.map(nonzero).transpose()?,
        state: v.state.map(|s| HostViewState {
            selected: s.selected_item_id.map(|x| owned(owner, x)),
            scope: s.scope_id.map(crikey_core::result_presentation::ScopeId),
            input: s.input.map(|x| executable(owner, x)),
        }),
        query: v.query,
        scroll: v.anchor_item_id.map(|x| ScrollAnchor {
            item: owned(owner, x),
            row_offset: v.anchor_row_offset,
        }),
        reason: match v.reason {
            1 => ViewRefreshReason::Explicit,
            2 => ViewRefreshReason::Query,
            3 => ViewRefreshReason::Selection,
            4 => ViewRefreshReason::Scope,
            _ => return Err(invalid()),
        },
    };
    result.validate().map_err(|_| invalid())?;
    Ok(result)
}
pub fn to_request(v: &ViewRequest) -> Result<m::StructuredViewRequest, ProtocolError> {
    v.validate().map_err(|_| invalid())?;
    Ok(m::StructuredViewRequest {
        route_id: v.route.0.clone(),
        logical_view: v.session.logical_view.get(),
        surface: v.session.surface.get(),
        mode: match v.mode {
            ViewRequestMode::Open => 1,
            ViewRequestMode::Refresh => 2,
        },
        previous_revision: v.previous_revision.map(NonZeroU64::get),
        state: v.state.as_ref().map(|s| m::ViewState {
            selected_item_id: s.selected.as_ref().map(|x| x.item.0.clone()),
            scope_id: s.scope.as_ref().map(|x| x.0.clone()),
            input: s.input.as_ref().map(wire_executable),
            ..Default::default()
        }),
        query: v.query.clone(),
        anchor_item_id: v.scroll.as_ref().map(|x| x.item.item.0.clone()),
        anchor_row_offset: v.scroll.as_ref().map_or(0, |x| x.row_offset),
        reason: match v.reason {
            ViewRefreshReason::Explicit => 1,
            ViewRefreshReason::Query => 2,
            ViewRefreshReason::Selection => 3,
            ViewRefreshReason::Scope => 4,
        },
        ..Default::default()
    })
}
fn from_menu(v: m::ViewMenuEntry) -> Result<ActionMenuEntry, ProtocolError> {
    match v.kind {
        1 if v.entries.is_empty()
            && v.id.is_empty()
            && v.label.is_empty()
            && v.description.is_empty()
            && v.icon_reference.is_none() =>
        {
            Ok(ActionMenuEntry::Action(ExecutablePresentation {
                action: v.action_id.map(ActionId),
                schema: v.schema.map(crate::typed::from_schema).transpose()?.map(Arc::new),
                shortcut: v.shortcut,
                destructive: v.destructive,
            }))
        }
        2 if v.action_id.is_none() && v.schema.is_none() && v.shortcut.is_none() && !v.destructive => {
            Ok(ActionMenuEntry::Submenu {
                id: v.id,
                label: v.label,
                description: v.description,
                icon_reference: v.icon_reference,
                entries: v.entries.into_iter().map(from_menu).collect::<Result<_, _>>()?,
            })
        }
        _ => Err(invalid()),
    }
}
fn to_menu(v: &ActionMenuEntry) -> Result<m::ViewMenuEntry, ProtocolError> {
    Ok(match v {
        ActionMenuEntry::Action(x) => m::ViewMenuEntry {
            kind: 1,
            action_id: x.action.as_ref().map(|x| x.0.clone()),
            schema: x
                .schema
                .as_ref()
                .map(|x| crate::typed::to_schema(x))
                .transpose()?,
            shortcut: x.shortcut.clone(),
            destructive: x.destructive,
            ..Default::default()
        },
        ActionMenuEntry::Submenu {
            id,
            label,
            description,
            icon_reference,
            entries,
        } => m::ViewMenuEntry {
            kind: 2,
            id: id.clone(),
            label: label.clone(),
            description: description.clone(),
            icon_reference: icon_reference.clone(),
            entries: entries.iter().map(to_menu).collect::<Result<_, _>>()?,
            ..Default::default()
        },
    })
}
fn from_collection(v: m::ViewCollection, owner: &PluginId) -> Result<CollectionView, ProtocolError> {
    use crikey_core::result_presentation::*;
    crate::view_budget::decoded_collection(&v)?;
    let mut items = Vec::with_capacity(v.items.len());
    for entry in v.items {
        let i = entry.item.ok_or_else(invalid)?;
        items.push(ItemPresentation {
            target: owned(owner, i.item_id),
            section: i.section_id.map(SectionId),
            scopes: i.scope_ids.into_iter().map(ScopeId).collect(),
            image: entry.image.map(from_image).transpose()?.map(Arc::new),
        });
    }
    Ok(CollectionView {
        items: items.into(),
        sections: v
            .sections
            .into_iter()
            .map(|s| Section {
                id: SectionId(s.id),
                label: s.label,
            })
            .collect(),
        scopes: v
            .scopes
            .into_iter()
            .map(|s| Scope {
                id: ScopeId(s.id),
                label: s.label,
            })
            .collect(),
        initial_selection: v.initial_selection.map(|x| owned(owner, x)),
        initial_scope: v.initial_scope.map(ScopeId),
    })
}
fn to_collection(v: &CollectionView) -> Result<m::ViewCollection, ProtocolError> {
    let mut items = Vec::with_capacity(v.items.len());
    for i in v.items.iter() {
        items.push(m::ViewItemPresentation {
            item: Some(m::ItemPresentation {
                item_id: i.target.item.0.clone(),
                section_id: i.section.as_ref().map(|x| x.0.clone()),
                scope_ids: i.scopes.iter().map(|x| x.0.clone()).collect(),
                ..Default::default()
            }),
            image: i.image.as_ref().map(|x| to_image(x)).transpose()?,
            ..Default::default()
        });
    }
    Ok(m::ViewCollection {
        items,
        sections: v
            .sections
            .iter()
            .map(|s| m::PresentationDeclaration {
                id: s.id.0.clone(),
                label: s.label.clone(),
                ..Default::default()
            })
            .collect(),
        scopes: v
            .scopes
            .iter()
            .map(|s| m::PresentationDeclaration {
                id: s.id.0.clone(),
                label: s.label.clone(),
                ..Default::default()
            })
            .collect(),
        initial_selection: v.initial_selection.as_ref().map(|x| x.item.0.clone()),
        initial_scope: v.initial_scope.as_ref().map(|x| x.0.clone()),
        ..Default::default()
    })
}
fn from_image(v: m::PreviewResponse) -> Result<crikey_core::PreviewImage, ProtocolError> {
    if !v.text.is_empty() || !v.values.is_empty() || !v.detail.is_empty() || v.markdown {
        return Err(invalid());
    }
    match crate::preview::from_response(v).map_err(|_| invalid())? {
        PreviewOutcome::Content(c) => c.image.ok_or_else(invalid),
        _ => Err(invalid()),
    }
}
fn to_image(v: &crikey_core::PreviewImage) -> Result<m::PreviewResponse, ProtocolError> {
    v.validate().map_err(|_| invalid())?;
    Ok(m::PreviewResponse {
        status: 1,
        width: v.width,
        height: v.height,
        rgba: v.rgba.clone(),
        ..Default::default()
    })
}
fn from_item(v: m::Item, owner: &PluginId) -> Result<crikey_core::Item, ProtocolError> {
    if v.stable_id.is_empty()
        || !matches!(v.argument_policy.as_str(), "forbidden" | "optional" | "required")
        || !matches!(v.hit_policy.as_str(), "recorded" | "ignored")
    {
        return Err(invalid());
    }
    let mut actions = Vec::with_capacity(v.actions.len());
    for a in v.actions {
        if a.execution_policy != "plugin" {
            return Err(invalid());
        }
        actions.push(crikey_core::Action {
            action_id: ActionId(a.action_id),
            label: a.label,
            description: a.description,
            icon_reference: (!a.icon_reference.is_empty()).then_some(a.icon_reference),
            execution_policy: ExecutionPolicy::Plugin,
            applicable_categories: a
                .applicable_categories
                .iter()
                .map(|c| crate::convert::category_from_tag(c))
                .collect(),
        });
    }
    Ok(crikey_core::Item {
        stable_id: ItemId(v.stable_id),
        plugin_id: owner.clone(),
        category: crate::convert::category_from_tag(&v.category),
        label: v.label,
        description: v.description,
        target: v.target,
        search_terms: v.search_terms,
        icon_reference: (!v.icon_reference.is_empty()).then_some(v.icon_reference),
        argument_policy: crate::convert::argument_policy_from_tag(&v.argument_policy),
        hit_policy: crate::convert::hit_policy_from_tag(&v.hit_policy),
        score_hint: v.score_hint,
        metadata: v.metadata,
        actions,
    })
}
pub fn from_descriptor(
    v: m::ViewDescriptor,
    admission: &RouteAdmission,
) -> Result<ViewDescriptor, ProtocolError> {
    let owner = &admission.identity().owner;
    let mut definitions = Vec::with_capacity(v.definitions.len());
    for d in v.definitions {
        let item = d.item.ok_or_else(invalid)?;
        let item = from_item(item, owner)?;
        definitions.push(RouteItemDefinition {
            item,
            actionless_policy: d.actionless.then_some(ExecutionPolicy::Plugin),
            primary_action: d.primary_action_id.map(ActionId),
            action_sections: d
                .action_sections
                .into_iter()
                .map(|s| {
                    Ok(ActionSection {
                        id: s.id,
                        label: s.label,
                        entries: s.entries.into_iter().map(from_menu).collect::<Result<_, _>>()?,
                    })
                })
                .collect::<Result<_, ProtocolError>>()?,
        });
    }
    let view = match v.kind {
        1 | 2 if v.detail.is_none() && v.detail_target.is_none() && v.form_submit.is_none() => {
            let c = from_collection(v.collection.ok_or_else(invalid)?, owner)?;
            if v.kind == 1 {
                if v.columns != 0 {
                    return Err(invalid());
                }
                StandardView::List(c)
            } else {
                StandardView::Grid(GridView {
                    collection: c,
                    columns: u8::try_from(v.columns).map_err(|_| invalid())?,
                })
            }
        }
        3 if v.collection.is_none() && v.columns == 0 && v.form_submit.is_none() => {
            let content =
                match crate::preview::from_response(v.detail.ok_or_else(invalid)?).map_err(|_| invalid())? {
                    PreviewOutcome::Content(c) => c,
                    _ => return Err(invalid()),
                };
            StandardView::Detail(
                DetailView::prepare(content, v.detail_target.map(|x| owned(owner, x)))
                    .map_err(|_| invalid())?,
            )
        }
        4 if v.collection.is_none() && v.columns == 0 && v.detail.is_none() && v.detail_target.is_none() => {
            StandardView::Form(FormView {
                submit: executable(owner, v.form_submit.ok_or_else(invalid)?),
            })
        }
        _ => return Err(invalid()),
    };
    let result = ViewDescriptor {
        title: v.title,
        definitions: definitions.into(),
        view,
    };
    result.validate(admission).map_err(|_| invalid())?;
    Ok(result)
}
pub fn to_descriptor(
    v: &ViewDescriptor,
    admission: &RouteAdmission,
) -> Result<m::ViewDescriptor, ProtocolError> {
    v.validate(admission).map_err(|_| invalid())?;
    crate::view_budget::outgoing_descriptor(v)?;
    let mut result = m::ViewDescriptor {
        title: v.title.clone(),
        ..Default::default()
    };
    for d in v.definitions.iter() {
        result.definitions.push(m::ViewItemDefinition {
            item: Some(crate::convert::to_proto_item(&d.item)),
            actionless: d.actionless_policy.is_some(),
            primary_action_id: d.primary_action.as_ref().map(|x| x.0.clone()),
            action_sections: d
                .action_sections
                .iter()
                .map(|s| {
                    Ok(m::ViewActionSection {
                        id: s.id.clone(),
                        label: s.label.clone(),
                        entries: s.entries.iter().map(to_menu).collect::<Result<_, _>>()?,
                        ..Default::default()
                    })
                })
                .collect::<Result<_, ProtocolError>>()?,
            ..Default::default()
        });
    }
    match &v.view {
        StandardView::List(c) => {
            result.kind = 1;
            result.collection = Some(to_collection(c)?);
        }
        StandardView::Grid(g) => {
            result.kind = 2;
            result.collection = Some(to_collection(&g.collection)?);
            result.columns = u32::from(g.columns);
        }
        StandardView::Detail(d) => {
            result.kind = 3;
            result.detail = Some(
                crate::preview::to_response(PreviewOutcome::Content(d.content().as_ref().clone()))
                    .map_err(|_| invalid())?,
            );
            result.detail_target = d.target.as_ref().map(|x| x.item.0.clone());
        }
        StandardView::Form(f) => {
            result.kind = 4;
            result.form_submit = Some(wire_executable(&f.submit));
        }
    }
    Ok(result)
}

fn u(v: &crate::wire::UnknownFields) -> usize {
    v.as_bytes().len().saturating_mul(2)
}
fn repetition<T>(n: usize) -> usize {
    let mut c = crate::wire::RepeatedFieldCharge::new::<T>();
    for _ in 0..n {
        c.push();
    }
    c.charged()
}
fn opt(v: &Option<String>) -> usize {
    v.as_ref().map_or(0, String::len)
}
fn check_text(v: &str, max: usize) -> Result<(), ProtocolError> {
    if v.len() > max {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn preview_charge(v: &m::PreviewResponse) -> Result<usize, ProtocolError> {
    if v.rgba.len() > crikey_core::preview::MAX_PREVIEW_IMAGE_BYTES
        || v.encoded_png.len() > crikey_core::preview::MAX_PREVIEW_IMAGE_BYTES
        || v.values.len() > 128
        || v.width > 1024
        || v.height > 1024
    {
        return Err(invalid());
    }
    if !v.encoded_png.is_empty() && (!v.rgba.is_empty() || v.width != 0 || v.height != 0) {
        return Err(invalid());
    }
    if !v.rgba.is_empty()
        && (v.width as usize)
            .checked_mul(v.height as usize)
            .and_then(|n| n.checked_mul(4))
            != Some(v.rgba.len())
    {
        return Err(invalid());
    }
    check_text(&v.text, 262144)?;
    check_text(&v.detail, 8192)?;
    let mut n = u(&v.unknown)
        + v.text.len()
        + v.detail.len()
        + v.rgba.len()
        + v.encoded_png.len()
        + repetition::<m::PreviewValue>(v.values.len());
    for x in &v.values {
        check_text(&x.label, 8192)?;
        check_text(&x.value, 8192)?;
        n = n.saturating_add(u(&x.unknown) + x.label.len() + x.value.len());
    }
    Ok(n)
}
fn menu_charge(v: &m::ViewMenuEntry, depth: usize, count: &mut usize) -> Result<usize, ProtocolError> {
    *count += 1;
    if depth > 4 || *count > 256 || v.entries.len() > 256 {
        return Err(invalid());
    }
    check_text(&v.id, 256)?;
    check_text(&v.label, 8192)?;
    check_text(&v.description, 8192)?;
    for x in [&v.action_id, &v.shortcut, &v.icon_reference]
        .into_iter()
        .flatten()
    {
        check_text(x, 8192)?;
    }
    let mut n = u(&v.unknown)
        + v.id.len()
        + v.label.len()
        + v.description.len()
        + opt(&v.action_id)
        + opt(&v.shortcut)
        + opt(&v.icon_reference)
        + repetition::<m::ViewMenuEntry>(v.entries.len());
    if let Some(s) = &v.schema {
        crate::typed::check_schema(s)?;
        n = n.saturating_add(crate::typed::schema_charge(s));
    }
    for e in &v.entries {
        n = n.saturating_add(menu_charge(e, depth + 1, count)?);
    }
    Ok(n)
}
pub fn response_charge(v: &m::StructuredViewResponse) -> Result<usize, ProtocolError> {
    check_text(&v.route_id, 256)?;
    let mut n = u(&v.unknown) + v.route_id.len();
    if let Some(d) = &v.descriptor {
        n = n.saturating_add(std::mem::size_of::<m::ViewDescriptor>());
        if v.status != 1 || d.definitions.len() > 4096 {
            return Err(invalid());
        }
        check_text(&d.title, 8192)?;
        n = n.saturating_add(
            u(&d.unknown)
                + d.title.len()
                + opt(&d.detail_target)
                + repetition::<m::ViewItemDefinition>(d.definitions.len()),
        );
        for def in &d.definitions {
            if def.action_sections.len() > 128 {
                return Err(invalid());
            }
            let item = def.item.as_ref().ok_or_else(invalid)?;
            if item.actions.len() > 256 || item.search_terms.len() > 128 || item.metadata.len() > 128 {
                return Err(invalid());
            }
            for s in [
                &item.stable_id,
                &item.label,
                &item.description,
                &item.target,
                &item.category,
                &item.icon_reference,
                &item.argument_policy,
                &item.hit_policy,
            ] {
                check_text(s, 8192)?;
            }
            for s in &item.search_terms {
                check_text(s, 8192)?;
            }
            for (k, v) in &item.metadata {
                check_text(k, 8192)?;
                check_text(v, 8192)?;
            }
            for a in &item.actions {
                if a.applicable_categories.len() > 128 {
                    return Err(invalid());
                }
                for s in [
                    &a.action_id,
                    &a.label,
                    &a.description,
                    &a.icon_reference,
                    &a.execution_policy,
                ]
                .into_iter()
                .chain(a.applicable_categories.iter())
                {
                    check_text(s, 8192)?;
                }
            }
            n = n.saturating_add(
                u(&def.unknown)
                    + item.decode_charge()
                    + opt(&def.primary_action_id)
                    + repetition::<m::ViewActionSection>(def.action_sections.len()),
            );
            let mut count = 0;
            for s in &def.action_sections {
                check_text(&s.id, 256)?;
                check_text(&s.label, 8192)?;
                n = n.saturating_add(
                    u(&s.unknown)
                        + s.id.len()
                        + s.label.len()
                        + repetition::<m::ViewMenuEntry>(s.entries.len()),
                );
                for e in &s.entries {
                    n = n.saturating_add(menu_charge(e, 1, &mut count)?);
                }
            }
        }
        if let Some(c) = &d.collection {
            if c.items.len() > 4096 || c.sections.len() > 128 || c.scopes.len() > 128 {
                return Err(invalid());
            }
            n = n.saturating_add(
                u(&c.unknown)
                    + opt(&c.initial_selection)
                    + opt(&c.initial_scope)
                    + repetition::<m::ViewItemPresentation>(c.items.len())
                    + repetition::<m::PresentationDeclaration>(c.sections.len())
                    + repetition::<m::PresentationDeclaration>(c.scopes.len()),
            );
            for x in c.sections.iter().chain(&c.scopes) {
                check_text(&x.id, 256)?;
                check_text(&x.label, 8192)?;
                n = n.saturating_add(u(&x.unknown) + x.id.len() + x.label.len());
            }
            for entry in &c.items {
                let x = entry.item.as_ref().ok_or_else(invalid)?;
                if x.scope_ids.len() > 128 {
                    return Err(invalid());
                }
                check_text(&x.item_id, 256)?;
                if let Some(s) = &x.section_id {
                    check_text(s, 256)?;
                }
                let mut membership = u(&x.unknown)
                    + x.item_id.len()
                    + opt(&x.section_id)
                    + repetition::<String>(x.scope_ids.len());
                for s in &x.scope_ids {
                    check_text(s, 256)?;
                    membership = membership.saturating_add(s.len());
                }
                if membership > 65536 {
                    return Err(invalid());
                }
                let mut local = u(&entry.unknown) + membership;
                if let Some(i) = &entry.image {
                    local = local.saturating_add(preview_charge(i)?);
                }
                if local > 2097152 {
                    return Err(invalid());
                }
                n = n.saturating_add(local);
            }
        }
        if let Some(p) = &d.detail {
            n = n.saturating_add(preview_charge(p)?);
        }
        if let Some(f) = &d.form_submit {
            n = n.saturating_add(u(&f.unknown) + f.item_id.len() + opt(&f.action_id));
        }
    } else if !matches!(v.status, 2..=5) {
        return Err(invalid());
    }
    Ok(n)
}

pub fn check_context(v: &m::ViewExecutionContext) -> Result<(), ProtocolError> {
    if v.route_id.is_empty()
        || v.route_id.len() > 256
        || v.route_id.chars().any(char::is_control)
        || v.logical_view == 0
        || v.surface == 0
        || v.revision == 0
        || u(&v.unknown).saturating_add(v.route_id.len()) > 1024
    {
        Err(invalid())
    } else {
        Ok(())
    }
}
pub fn check_navigation(v: &m::RouteNavigation) -> Result<(), ProtocolError> {
    if !matches!(v.kind, 1 | 2)
        || v.route_id.is_empty()
        || v.route_id.len() > 256
        || v.route_id.chars().any(char::is_control)
        || u(&v.unknown).saturating_add(v.route_id.len()) > 1024
    {
        Err(invalid())
    } else {
        Ok(())
    }
}
pub fn request_charge(v: &m::StructuredViewRequest) -> Result<usize, ProtocolError> {
    if v.route_id.is_empty()
        || v.route_id.len() > 256
        || v.route_id.chars().any(char::is_control)
        || v.logical_view == 0
        || v.surface == 0
        || v.query.len() > 8192
        || !matches!(v.reason, 1..=4)
        || !matches!((v.mode, v.previous_revision), (1, None) | (2, Some(1..)))
        || (v.mode == 2 && v.state.is_none())
    {
        return Err(invalid());
    }
    if let Some(anchor) = &v.anchor_item_id {
        check_text(anchor, 256)?;
    } else if v.anchor_row_offset != 0 {
        return Err(invalid());
    }
    let mut n = u(&v.unknown) + v.route_id.len() + v.query.len() + opt(&v.anchor_item_id);
    if let Some(s) = &v.state {
        let mut state = u(&s.unknown) + opt(&s.selected_item_id) + opt(&s.scope_id);
        for x in [&s.selected_item_id, &s.scope_id].into_iter().flatten() {
            check_text(x, 256)?;
        }
        if let Some(e) = &s.input {
            check_text(&e.item_id, 256)?;
            if let Some(a) = &e.action_id {
                check_text(a, 256)?;
            }
            let executable = u(&e.unknown) + e.item_id.len() + opt(&e.action_id);
            if executable > 1024 {
                return Err(invalid());
            }
            state = state.saturating_add(executable);
        }
        if state > 4096 {
            return Err(invalid());
        }
        n = n.saturating_add(state);
    }
    if n > 16384 {
        return Err(invalid());
    }
    Ok(n)
}

/// The only credit-exempt view reply is one bounded, awaited terminal refusal.
/// The host separately enforces its exact outstanding request and connection.
pub fn bounded_refusal(envelope: &m::Envelope) -> bool {
    let Some(m::Payload::StructuredViewResponse(response)) = &envelope.payload else {
        return false;
    };
    matches!(response.status, 2..=5)
        && response.descriptor.is_none()
        && !response.route_id.is_empty()
        && response.route_id.len() <= 256
        && response.logical_view != 0
        && response.surface != 0
        && response.decoded_bytes <= 960
        && u(&envelope.unknown)
            .saturating_add(u(&response.unknown))
            .saturating_add(response.route_id.len())
            .saturating_add(64)
            <= 1024
}
