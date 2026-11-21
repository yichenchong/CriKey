//! Native PR7 typed arguments and independently negotiated root presentation.
//! Endpoint ownership is supplied by the host, never by a plugin message.
use crate::{message as m, ProtocolError};
use crikey_core::result_presentation as p;
use crikey_core::structured_arguments as a;
use crikey_core::{ActionId, ItemId, PluginId};
use std::sync::Arc;

fn invalid() -> ProtocolError {
    ProtocolError::Malformed("invalid typed payload".into())
}
fn id(value: &str) -> Result<(), ProtocolError> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn text(value: &str) -> Result<(), ProtocolError> {
    if value.len() > 8192 {
        Err(invalid())
    } else {
        Ok(())
    }
}

pub fn from_schema(schema: m::ArgumentSchema) -> Result<a::ArgumentSchema, ProtocolError> {
    check_schema(&schema)?;
    let mut fields = Vec::with_capacity(schema.fields.len());
    for field in schema.fields {
        let min_bytes = field.min_bytes as usize;
        let max_bytes = field.max_bytes.unwrap_or(8192) as usize;
        let kind = match field.kind {
            1 => a::FieldKind::Text {
                min_bytes,
                max_bytes,
                default: field.default_value,
                multiline: field.multiline,
            },
            2 => a::FieldKind::Password { min_bytes, max_bytes },
            3 => a::FieldKind::Dropdown {
                options: field
                    .options
                    .into_iter()
                    .map(|option| a::DropdownOption {
                        id: a::OptionId(option.id),
                        label: option.label,
                    })
                    .collect(),
                default: field.default_value.map(a::OptionId),
            },
            _ => return Err(invalid()),
        };
        fields.push(a::ArgumentField {
            id: a::FieldId(field.id),
            label: field.label,
            required: field.required,
            placeholder: field.placeholder,
            validation_message: field.validation_message,
            kind,
        });
    }
    let schema = a::ArgumentSchema { fields };
    schema.validate().map_err(|_| invalid())?;
    Ok(schema)
}
pub fn to_schema(schema: &a::ArgumentSchema) -> Result<m::ArgumentSchema, ProtocolError> {
    schema.validate().map_err(|_| invalid())?;
    let fields = schema
        .fields
        .iter()
        .map(|field| {
            let (kind, min_bytes, max_bytes, multiline, default_value, options) = match &field.kind {
                a::FieldKind::Text {
                    min_bytes,
                    max_bytes,
                    default,
                    multiline,
                } => (1, *min_bytes, *max_bytes, *multiline, default.clone(), Vec::new()),
                a::FieldKind::Password { min_bytes, max_bytes } => {
                    (2, *min_bytes, *max_bytes, false, None, Vec::new())
                }
                a::FieldKind::Dropdown { options, default } => (
                    3,
                    0,
                    8192,
                    false,
                    default.as_ref().map(|id| id.0.clone()),
                    options
                        .iter()
                        .map(|o| m::DropdownOption {
                            id: o.id.0.clone(),
                            label: o.label.clone(),
                            ..Default::default()
                        })
                        .collect(),
                ),
            };
            m::ArgumentField {
                id: field.id.0.clone(),
                label: field.label.clone(),
                required: field.required,
                placeholder: field.placeholder.clone(),
                validation_message: field.validation_message.clone(),
                kind,
                min_bytes: min_bytes as u32,
                max_bytes: Some(max_bytes as u32),
                multiline,
                default_value,
                options,
                ..Default::default()
            }
        })
        .collect();
    let result = m::ArgumentSchema {
        fields,
        ..Default::default()
    };
    check_schema(&result)?;
    Ok(result)
}
pub fn from_presentation(
    value: m::ResultPresentation,
    owner: &PluginId,
) -> Result<p::ResultPresentation, ProtocolError> {
    check_presentation(&value)?;
    id(&owner.0)?;
    let owned = |item: String| a::OwnedItem {
        owner: owner.clone(),
        item: ItemId(item),
    };
    let sections = value
        .sections
        .into_iter()
        .map(|v| p::Section {
            id: p::SectionId(v.id),
            label: v.label,
        })
        .collect();
    let scopes = value
        .scopes
        .into_iter()
        .map(|v| p::Scope {
            id: p::ScopeId(v.id),
            label: v.label,
        })
        .collect();
    let items = value
        .items
        .into_iter()
        .map(|v| p::ItemPresentation {
            target: owned(v.item_id),
            section: v.section_id.map(p::SectionId),
            scopes: v.scope_ids.into_iter().map(p::ScopeId).collect(),
            image: None,
        })
        .collect();
    let mut schemas = Vec::with_capacity(value.schemas.len());
    for binding in value.schemas {
        schemas.push(p::ExecutableSchemaBinding {
            executable: a::ExecutableKey {
                target: owned(binding.item_id),
                action: binding.action_id.map(ActionId),
            },
            schema: Arc::new(from_schema(binding.schema.ok_or_else(invalid)?)?),
        });
    }
    Ok(p::ResultPresentation {
        sections,
        scopes,
        items,
        schemas,
    })
}
pub fn to_presentation(value: p::ResultPresentation) -> Result<m::ResultPresentation, ProtocolError> {
    // Validate limits without cloning. Full item/action reference admission is
    // performed transactionally by the receiver against its complete result set.
    if value.sections.len() > 128
        || value.scopes.len() > 128
        || value.items.len() > 4096
        || value.schemas.len() > 4096
    {
        return Err(invalid());
    }
    let mut budget = crikey_core::content_accounting::ContentFootprint::new(4 * 1024 * 1024, 0);
    value.account(&mut budget).map_err(|_| invalid())?;
    let mut result = m::ResultPresentation {
        sections: value
            .sections
            .into_iter()
            .map(|v| m::PresentationDeclaration {
                id: v.id.0,
                label: v.label,
                ..Default::default()
            })
            .collect(),
        scopes: value
            .scopes
            .into_iter()
            .map(|v| m::PresentationDeclaration {
                id: v.id.0,
                label: v.label,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    for item in value.items {
        if item.image.is_some() {
            return Err(invalid());
        }
        result.items.push(m::ItemPresentation {
            item_id: item.target.item.0,
            section_id: item.section.map(|v| v.0),
            scope_ids: item.scopes.into_iter().map(|v| v.0).collect(),
            ..Default::default()
        });
    }
    for binding in value.schemas {
        result.schemas.push(m::ExecutableSchemaBinding {
            item_id: binding.executable.target.item.0,
            action_id: binding.executable.action.map(|v| v.0),
            schema: Some(to_schema(&binding.schema)?),
            ..Default::default()
        });
    }
    check_presentation(&result)?;
    Ok(result)
}
pub fn to_request(
    item: &ItemId,
    action: Option<&ActionId>,
    expected: [u8; 32],
    values: a::ArgumentValues,
) -> Result<m::TypedExecuteRequest, ProtocolError> {
    id(&item.0)?;
    if let Some(action) = action {
        id(&action.0)?;
    }
    let result = m::TypedExecuteRequest {
        item_id: item.0.clone(),
        action_id: action.map(|v| v.0.clone()),
        expected_fingerprint: expected.to_vec(),
        values,
        ..Default::default()
    };
    check_request(&result)?;
    Ok(result)
}
pub fn from_response(
    response: m::TypedExecuteResponse,
    schema: &a::ArgumentSchema,
) -> Result<a::SubmissionOutcome, ProtocolError> {
    use a::{SubmissionFailureCode as F, SubmissionFieldCode as C, SubmissionOutcome as O};
    if response.failures.len() > 64 {
        return Err(invalid());
    }
    let outcome = match response.status {
        1 if response.failures.is_empty() && response.failure_code == 0 => O::Completed,
        2 if response.failure_code == 0 => {
            let mut errors = Vec::with_capacity(response.failures.len());
            for failure in response.failures {
                let code = match failure.code {
                    1 => C::Required,
                    2 => C::TooShort,
                    3 => C::TooLong,
                    4 => C::InvalidSelection,
                    5 => C::Rejected,
                    _ => return Err(invalid()),
                };
                errors.push(a::SubmissionFieldFailure {
                    field: a::FieldId(failure.field_id),
                    code,
                });
            }
            O::InvalidFields(errors)
        }
        3 if response.failures.is_empty() => O::Failed(match response.failure_code {
            1 => F::Unavailable,
            2 => F::Denied,
            3 => F::Cancelled,
            4 => F::Timeout,
            5 => F::Unsupported,
            6 => F::Failed,
            7 => F::SchemaMismatch,
            8 => F::ResourceLimit,
            9 => F::ProtocolViolation,
            _ => return Err(invalid()),
        }),
        _ => return Err(invalid()),
    };
    outcome.validate(Some(schema)).map_err(|_| invalid())?;
    Ok(outcome)
}
pub fn to_response(
    outcome: a::SubmissionOutcome,
    schema: &a::ArgumentSchema,
) -> Result<m::TypedExecuteResponse, ProtocolError> {
    use a::{SubmissionFailureCode as F, SubmissionFieldCode as C, SubmissionOutcome as O};
    outcome.validate(Some(schema)).map_err(|_| invalid())?;
    let mut response = m::TypedExecuteResponse::default();
    match outcome {
        O::Completed => response.status = 1,
        O::InvalidFields(failures) => {
            response.status = 2;
            response.failures = failures
                .into_iter()
                .map(|f| m::TypedFieldFailure {
                    field_id: f.field.0,
                    code: match f.code {
                        C::Required => 1,
                        C::TooShort => 2,
                        C::TooLong => 3,
                        C::InvalidSelection => 4,
                        C::Rejected => 5,
                    },
                    ..Default::default()
                })
                .collect();
        }
        O::Failed(code) => {
            response.status = 3;
            response.failure_code = match code {
                F::Unavailable => 1,
                F::Denied => 2,
                F::Cancelled => 3,
                F::Timeout => 4,
                F::Unsupported => 5,
                F::Failed => 6,
                F::SchemaMismatch => 7,
                F::ResourceLimit => 8,
                F::ProtocolViolation => 9,
            };
        }
    }
    Ok(response)
}

// Producer accounting mirrors the decoder's growing vectors and charges all
// unknown bytes. It runs before encoding allocates an output/nested buffer.
fn unknown(value: &crate::wire::UnknownFields) -> usize {
    value.as_bytes().len().saturating_mul(2)
}
fn vec_charge<T>(len: usize) -> usize {
    let mut c = crate::wire::RepeatedFieldCharge::new::<T>();
    for _ in 0..len {
        c.push();
    }
    c.charged()
}
fn optional(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, |s| s.len())
}
pub fn schema_charge(schema: &m::ArgumentSchema) -> usize {
    let mut n = unknown(&schema.unknown).saturating_add(vec_charge::<m::ArgumentField>(schema.fields.len()));
    for f in &schema.fields {
        n = n
            .saturating_add(unknown(&f.unknown))
            .saturating_add(f.id.len())
            .saturating_add(f.label.len())
            .saturating_add(optional(&f.placeholder))
            .saturating_add(optional(&f.validation_message))
            .saturating_add(optional(&f.default_value))
            .saturating_add(vec_charge::<m::DropdownOption>(f.options.len()));
        for o in &f.options {
            n = n
                .saturating_add(o.id.len())
                .saturating_add(o.label.len())
                .saturating_add(unknown(&o.unknown));
        }
    }
    n
}
pub fn check_schema(schema: &m::ArgumentSchema) -> Result<(), ProtocolError> {
    if schema.fields.len() > 64 || schema_charge(schema) > 262144 {
        return Err(invalid());
    }
    for (i, f) in schema.fields.iter().enumerate() {
        id(&f.id)?;
        text(&f.label)?;
        if schema.fields[..i].iter().any(|v| v.id == f.id) {
            return Err(invalid());
        }
        for s in [&f.placeholder, &f.validation_message, &f.default_value]
            .into_iter()
            .flatten()
        {
            text(s)?;
        }
        let max = f.max_bytes.unwrap_or(8192);
        if f.min_bytes > max || max > 8192 {
            return Err(invalid());
        }
        match f.kind {
            1 if f.options.is_empty() => {}
            2 if f.default_value.is_none() && f.options.is_empty() && !f.multiline => {}
            3 if !f.options.is_empty()
                && f.options.len() <= 256
                && f.min_bytes == 0
                && max == 8192
                && !f.multiline => {}
            _ => return Err(invalid()),
        }
        for (j, o) in f.options.iter().enumerate() {
            id(&o.id)?;
            text(&o.label)?;
            if f.options[..j].iter().any(|v| v.id == o.id) {
                return Err(invalid());
            }
        }
        if let Some(default) = &f.default_value {
            match f.kind {
                1 if default.len() > max as usize
                    || (default.is_empty() && f.required)
                    || (!default.is_empty() && default.len() < f.min_bytes as usize) =>
                {
                    return Err(invalid())
                }
                3 if !f.options.iter().any(|o| o.id == *default) => return Err(invalid()),
                _ => {}
            }
        }
    }
    Ok(())
}
pub fn presentation_charge(value: &m::ResultPresentation) -> usize {
    let mut n = unknown(&value.unknown)
        .saturating_add(vec_charge::<m::PresentationDeclaration>(value.sections.len()))
        .saturating_add(vec_charge::<m::PresentationDeclaration>(value.scopes.len()))
        .saturating_add(vec_charge::<m::ItemPresentation>(value.items.len()))
        .saturating_add(vec_charge::<m::ExecutableSchemaBinding>(value.schemas.len()));
    for d in value.sections.iter().chain(&value.scopes) {
        n = n
            .saturating_add(d.id.len())
            .saturating_add(d.label.len())
            .saturating_add(unknown(&d.unknown));
    }
    for i in &value.items {
        n = n
            .saturating_add(i.item_id.len())
            .saturating_add(optional(&i.section_id))
            .saturating_add(vec_charge::<String>(i.scope_ids.len()))
            .saturating_add(unknown(&i.unknown));
        for scope in &i.scope_ids {
            n = n.saturating_add(scope.len());
        }
    }
    for b in &value.schemas {
        n = n
            .saturating_add(b.item_id.len())
            .saturating_add(optional(&b.action_id))
            .saturating_add(unknown(&b.unknown));
        if let Some(s) = &b.schema {
            n = n.saturating_add(schema_charge(s));
        }
    }
    n
}
pub fn check_presentation(value: &m::ResultPresentation) -> Result<(), ProtocolError> {
    if value.sections.len() > 128
        || value.scopes.len() > 128
        || value.items.len() > 4096
        || value.schemas.len() > 4096
        || presentation_charge(value) > 4 * 1024 * 1024
    {
        return Err(invalid());
    }
    for declarations in [&value.sections, &value.scopes] {
        for (i, d) in declarations.iter().enumerate() {
            id(&d.id)?;
            text(&d.label)?;
            if declarations[..i].iter().any(|v| v.id == d.id) {
                return Err(invalid());
            }
        }
    }
    for (index, item) in value.items.iter().enumerate() {
        id(&item.item_id)?;
        if value.items[..index].iter().any(|v| v.item_id == item.item_id) {
            return Err(invalid());
        }
        if let Some(s) = &item.section_id {
            id(s)?;
        }
        if item.scope_ids.len() > 128 {
            return Err(invalid());
        }
        for (i, s) in item.scope_ids.iter().enumerate() {
            id(s)?;
            if item.scope_ids[..i].contains(s) {
                return Err(invalid());
            }
        }
    }
    for (i, b) in value.schemas.iter().enumerate() {
        id(&b.item_id)?;
        if let Some(a) = &b.action_id {
            id(a)?;
        }
        if value.schemas[..i]
            .iter()
            .any(|v| v.item_id == b.item_id && v.action_id == b.action_id)
        {
            return Err(invalid());
        }
        check_schema(b.schema.as_ref().ok_or_else(invalid)?)?;
    }
    Ok(())
}
pub fn request_charge(value: &m::TypedExecuteRequest) -> usize {
    let mut n = value
        .item_id
        .len()
        .saturating_add(optional(&value.action_id))
        .saturating_add(value.expected_fingerprint.len())
        .saturating_add(unknown(&value.unknown));
    for (id, v) in &value.values.0 {
        let length = match v {
            a::StructuredValue::Password(p) => p.expose_plaintext().len(),
            a::StructuredValue::NonSecret(a::NonSecretValue::Text(s)) => s.len(),
            a::StructuredValue::NonSecret(a::NonSecretValue::Dropdown(o)) => o.0.len(),
        };
        n = n
            .saturating_add(id.0.len())
            .saturating_add(length)
            .saturating_add(crate::wire::map_entry_charge());
    }
    for (id, bytes) in &value.value_unknown {
        n = n
            .saturating_add(id.0.len())
            .saturating_add(unknown(bytes))
            .saturating_add(crate::wire::map_entry_charge());
    }
    n
}
pub fn check_request(value: &m::TypedExecuteRequest) -> Result<(), ProtocolError> {
    id(&value.item_id)?;
    if let Some(a) = &value.action_id {
        id(a)?;
    }
    if value.expected_fingerprint.len() != 32
        || value.values.0.len() > 64
        || request_charge(value) > 1024 * 1024
    {
        return Err(invalid());
    }
    if value
        .value_unknown
        .keys()
        .any(|id| !value.values.0.contains_key(id))
    {
        return Err(invalid());
    }
    for (field, v) in &value.values.0 {
        id(&field.0)?;
        match v {
            a::StructuredValue::Password(p) => text(p.expose_plaintext())?,
            a::StructuredValue::NonSecret(a::NonSecretValue::Text(s)) => text(s)?,
            a::StructuredValue::NonSecret(a::NonSecretValue::Dropdown(o)) => id(&o.0)?,
        }
    }
    Ok(())
}
