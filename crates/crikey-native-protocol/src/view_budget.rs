//! Transient view conversion budgets. These are not another retention allowance.
use crate::{message as m, ProtocolError};
use crikey_core::structured_arguments::{ArgumentSchema, FieldKind};
use crikey_core::structured_view::*;
use crikey_core::{Category, PreviewContent, PreviewText};

const LIMIT: usize = crate::wire::DECODE_ALLOCATION_BUDGET;
struct Budget(usize);
impl Budget {
    fn charge(&mut self, bytes: usize) -> Result<(), ProtocolError> {
        let requested = self.0.saturating_add(bytes);
        if requested > LIMIT {
            return Err(ProtocolError::DecodeBudgetExceeded {
                requested,
                remaining: LIMIT.saturating_sub(self.0),
            });
        }
        self.0 = requested;
        Ok(())
    }
    fn repeated<T>(&mut self, len: usize) -> Result<(), ProtocolError> {
        let mut charge = crate::wire::RepeatedFieldCharge::new::<T>();
        for _ in 0..len {
            charge.push();
        }
        self.charge(charge.charged())
    }
    fn optional(&mut self, value: Option<&str>) -> Result<(), ProtocolError> {
        self.charge(value.map_or(0, str::len))
    }
}
fn invalid() -> ProtocolError {
    ProtocolError::Malformed("invalid structured view image".into())
}

// IHDR is the first PNG chunk. Reading its fixed-size dimensions does not run
// the PNG parser or allocate decoder metadata/output. The real decoder still
// validates CRCs, transforms, animation and pixels after this aggregate gate.
fn png_raster_bytes(bytes: &[u8]) -> Result<usize, ProtocolError> {
    if bytes.len() < 33
        || bytes.len() > crikey_core::preview::MAX_PREVIEW_IMAGE_BYTES
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || bytes[8..12] != 13u32.to_be_bytes()
        || &bytes[12..16] != b"IHDR"
    {
        return Err(invalid());
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().map_err(|_| invalid())?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().map_err(|_| invalid())?);
    raster_bytes(width, height)
}
fn raster_bytes(width: u32, height: u32) -> Result<usize, ProtocolError> {
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(invalid)?;
    if width == 0
        || height == 0
        || width > crikey_core::preview::MAX_PREVIEW_IMAGE_EDGE
        || height > crikey_core::preview::MAX_PREVIEW_IMAGE_EDGE
        || bytes > crikey_core::preview::MAX_PREVIEW_IMAGE_BYTES
    {
        return Err(invalid());
    }
    Ok(bytes)
}
/// Preflight the entire collection before expanding even its first PNG. Every
/// occurrence has distinct decoded storage; all compressed and raw buffers
/// remain live while converting. Reserve them before visiting any PNG, plus
/// the PNG library ceiling, normalized output and RGBA expansion together.
pub(crate) fn decoded_collection(value: &m::ViewCollection) -> Result<(), ProtocolError> {
    let mut existing = Budget(0);
    for entry in &value.items {
        if let Some(image) = &entry.image {
            existing.charge(image.encoded_png.capacity())?;
            existing.charge(image.rgba.capacity())?;
        }
    }
    let mut retained = Budget(existing.0);
    let mut pixels = 0usize;
    for entry in &value.items {
        let Some(image) = &entry.image else {
            continue;
        };
        if image.status != 1
            || !image.text.is_empty()
            || image.markdown
            || !image.values.is_empty()
            || !image.detail.is_empty()
        {
            return Err(invalid());
        }
        let size = if image.encoded_png.is_empty() {
            let size = raster_bytes(image.width, image.height)?;
            if image.rgba.len() != size {
                return Err(invalid());
            }
            size
        } else {
            if image.width != 0 || image.height != 0 || !image.rgba.is_empty() {
                return Err(invalid());
            }
            let size = png_raster_bytes(&image.encoded_png)?;
            let mut peak = Budget(retained.0);
            peak.charge(crikey_core::preview::MAX_PREVIEW_IMAGE_BYTES)?;
            peak.charge(size)?; // Normalized decoder output, before conversion.
            peak.charge(size)?; // RGBA allocation while normalized output is live.
            retained.charge(size)?;
            size
        };
        pixels = pixels.checked_add(size / 4).ok_or_else(invalid)?;
        if pixels > crikey_core::preview::MAX_RETAINED_PREVIEW_PIXELS {
            return Err(invalid());
        }
    }
    Ok(())
}
fn category_len(value: &Category) -> usize {
    match value {
        Category::PluginDefined(name) => crikey_core::PLUGIN_DEFINED_PREFIX.len() + name.len(),
        _ => value.as_str().len(),
    }
}
fn schema(budget: &mut Budget, value: &ArgumentSchema) -> Result<(), ProtocolError> {
    budget.repeated::<m::ArgumentField>(value.fields.len())?;
    for field in &value.fields {
        budget.charge(field.id.0.len())?;
        budget.charge(field.label.len())?;
        budget.optional(field.placeholder.as_deref())?;
        budget.optional(field.validation_message.as_deref())?;
        match &field.kind {
            FieldKind::Text { default, .. } => budget.optional(default.as_deref())?,
            FieldKind::Password { .. } => {}
            FieldKind::Dropdown { options, default } => {
                budget.optional(default.as_ref().map(|x| x.0.as_str()))?;
                budget.repeated::<m::DropdownOption>(options.len())?;
                for option in options {
                    budget.charge(option.id.0.len())?;
                    budget.charge(option.label.len())?;
                }
            }
        }
    }
    Ok(())
}
fn menu(budget: &mut Budget, entries: &[ActionMenuEntry]) -> Result<(), ProtocolError> {
    budget.repeated::<m::ViewMenuEntry>(entries.len())?;
    for entry in entries {
        match entry {
            ActionMenuEntry::Action(value) => {
                budget.optional(value.action.as_ref().map(|x| x.0.as_str()))?;
                budget.optional(value.shortcut.as_deref())?;
                if let Some(value) = &value.schema {
                    schema(budget, value)?;
                }
            }
            ActionMenuEntry::Submenu {
                id,
                label,
                description,
                icon_reference,
                entries,
            } => {
                budget.charge(id.len())?;
                budget.charge(label.len())?;
                budget.charge(description.len())?;
                budget.optional(icon_reference.as_deref())?;
                menu(budget, entries)?;
            }
        }
    }
    Ok(())
}
fn collection(budget: &mut Budget, value: &CollectionView) -> Result<(), ProtocolError> {
    budget.repeated::<m::ViewItemPresentation>(value.items.len())?;
    budget.repeated::<m::PresentationDeclaration>(value.sections.len())?;
    budget.repeated::<m::PresentationDeclaration>(value.scopes.len())?;
    for section in &value.sections {
        budget.charge(section.id.0.len())?;
        budget.charge(section.label.len())?;
    }
    for scope in &value.scopes {
        budget.charge(scope.id.0.len())?;
        budget.charge(scope.label.len())?;
    }
    budget.optional(value.initial_selection.as_ref().map(|x| x.item.0.as_str()))?;
    budget.optional(value.initial_scope.as_ref().map(|x| x.0.as_str()))?;
    for item in value.items.iter() {
        budget.charge(item.target.item.0.len())?;
        budget.optional(item.section.as_ref().map(|x| x.0.as_str()))?;
        budget.repeated::<String>(item.scopes.len())?;
        for scope in &item.scopes {
            budget.charge(scope.0.len())?;
        }
        if let Some(image) = &item.image {
            budget.charge(image.rgba.len())?;
        }
    }
    Ok(())
}
fn preview(budget: &mut Budget, value: &PreviewContent) -> Result<(), ProtocolError> {
    if let Some(PreviewText::Plain(text) | PreviewText::Markdown(text)) = &value.text {
        budget.charge(text.len())?;
    }
    budget.repeated::<m::PreviewValue>(value.values.len())?;
    for entry in &value.values {
        budget.charge(entry.label.len())?;
        budget.charge(entry.value.len())?;
    }
    if let Some(image) = &value.image {
        budget.charge(image.rgba.len())?;
    }
    Ok(())
}
/// Mirror the wire decoder's allocations by occurrence, not Core Arc identity.
/// Runs before cloning any descriptor strings, schemas or raster buffers.
pub(crate) fn outgoing_descriptor(value: &ViewDescriptor) -> Result<(), ProtocolError> {
    let mut budget = Budget(crate::message::BATCH_OVERHEAD_RESERVE);
    budget.charge(std::mem::size_of::<m::ViewDescriptor>())?;
    budget.charge(value.title.len())?;
    budget.repeated::<m::ViewItemDefinition>(value.definitions.len())?;
    for definition in value.definitions.iter() {
        let item = &definition.item;
        for text in [&item.stable_id.0, &item.label, &item.description, &item.target] {
            budget.charge(text.len())?;
        }
        budget.charge(category_len(&item.category))?;
        budget.optional(item.icon_reference.as_deref())?;
        budget.charge(crate::convert::argument_policy_tag(item.argument_policy).len())?;
        budget.charge(crate::convert::hit_policy_tag(item.hit_policy).len())?;
        budget.repeated::<String>(item.search_terms.len())?;
        for text in &item.search_terms {
            budget.charge(text.len())?;
        }
        for (key, value) in &item.metadata {
            budget.charge(crate::wire::map_entry_charge())?;
            budget.charge(key.len())?;
            budget.charge(value.len())?;
        }
        budget.repeated::<m::Action>(item.actions.len())?;
        for action in &item.actions {
            for text in [&action.action_id.0, &action.label, &action.description] {
                budget.charge(text.len())?;
            }
            budget.optional(action.icon_reference.as_deref())?;
            budget.charge("plugin".len())?;
            budget.repeated::<String>(action.applicable_categories.len())?;
            for category in &action.applicable_categories {
                budget.charge(category_len(category))?;
            }
        }
        budget.optional(definition.primary_action.as_ref().map(|x| x.0.as_str()))?;
        budget.repeated::<m::ViewActionSection>(definition.action_sections.len())?;
        for section in &definition.action_sections {
            budget.charge(section.id.len())?;
            budget.charge(section.label.len())?;
            menu(&mut budget, &section.entries)?;
        }
    }
    match &value.view {
        StandardView::List(value) => collection(&mut budget, value)?,
        StandardView::Grid(value) => collection(&mut budget, &value.collection)?,
        StandardView::Detail(value) => {
            preview(&mut budget, value.content())?;
            budget.optional(value.target.as_ref().map(|x| x.item.0.as_str()))?;
        }
        StandardView::Form(value) => {
            budget.charge(value.submit.target.item.0.len())?;
            budget.optional(value.submit.action.as_ref().map(|x| x.0.as_str()))?;
        }
    }
    Ok(())
}
