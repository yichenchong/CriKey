//! Modern JSON PR7 sidecars and fixed-code typed execution; no navigation outcomes.
use crikey_core::result_presentation::*;
use crikey_core::structured_arguments::*;
use crikey_core::{ActionId, Item, ItemId, PluginId};
use serde_json::{Map, Value};
use std::{collections::BTreeSet, sync::Arc};

fn string(map: &Map<String, Value>, key: &str) -> Option<String> {
    Some(map.get(key)?.as_str()?.to_owned())
}
fn optional(map: &Map<String, Value>, key: &str) -> Option<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(value) => Some(Some(value.as_str()?.to_owned())),
    }
}
fn array<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a [Value]> {
    match map.get(key) {
        None => Some(&[]),
        Some(value) => Some(value.as_array()?),
    }
}
fn boolean(map: &Map<String, Value>, key: &str) -> Option<bool> {
    map.get(key).map_or(Some(false), Value::as_bool)
}
fn integer(map: &Map<String, Value>, key: &str, default: usize) -> Option<usize> {
    map.get(key)
        .map_or(Some(default), |v| usize::try_from(v.as_u64()?).ok())
}

pub(crate) fn schema(value: &Value) -> Option<ArgumentSchema> {
    let map = value.as_object()?;
    let entries = array(map, "fields")?;
    let mut fields = Vec::with_capacity(entries.len());
    for entry in entries {
        let m = entry.as_object()?;
        let min_bytes = integer(m, "min_bytes", 0)?;
        let max_bytes = integer(m, "max_bytes", 8192)?;
        let kind = match m.get("kind")?.as_str()? {
            "text" => FieldKind::Text {
                min_bytes,
                max_bytes,
                multiline: boolean(m, "multiline")?,
                default: optional(m, "default")?,
            },
            "password" => {
                if m.contains_key("default") {
                    return None;
                }
                FieldKind::Password { min_bytes, max_bytes }
            }
            "dropdown" => {
                let entries = array(m, "options")?;
                let mut options = Vec::with_capacity(entries.len());
                for option in entries {
                    let option = option.as_object()?;
                    options.push(DropdownOption {
                        id: OptionId(string(option, "id")?),
                        label: string(option, "label")?,
                    });
                }
                FieldKind::Dropdown {
                    options,
                    default: optional(m, "default")?.map(OptionId),
                }
            }
            _ => return None,
        };
        fields.push(ArgumentField {
            id: FieldId(string(m, "id")?),
            label: string(m, "label")?,
            required: boolean(m, "required")?,
            placeholder: optional(m, "placeholder")?,
            validation_message: optional(m, "guidance")?,
            kind,
        });
    }
    let schema = ArgumentSchema { fields };
    schema.validate().ok()?;
    Some(schema)
}

pub(crate) fn append(
    owner: &PluginId,
    frame: &Map<String, Value>,
    out: &mut ResultPresentation,
    typed: bool,
    visual: bool,
    bytes: &mut usize,
) -> Option<()> {
    let Some(value) = frame.get("presentation") else {
        return Some(());
    };
    fn charge(value: &Value) -> Option<usize> {
        let mut bytes = 64usize;
        match value {
            Value::String(s) => bytes = bytes.checked_add(s.capacity())?,
            Value::Array(values) => {
                for value in values {
                    bytes = bytes.checked_add(charge(value)?)?;
                }
            }
            Value::Object(map) => {
                for (key, value) in map {
                    bytes = bytes.checked_add(key.capacity())?.checked_add(charge(value)?)?;
                }
            }
            _ => {}
        }
        Some(bytes)
    }
    *bytes = bytes.checked_add(charge(value)?)?;
    if *bytes > crate::protocol::MAX_FRAME_BYTES {
        return None;
    }
    let map = value.as_object()?;
    if visual {
        for value in array(map, "sections")? {
            let m = value.as_object()?;
            out.sections.push(Section {
                id: SectionId(string(m, "id")?),
                label: string(m, "label")?,
            });
        }
        for value in array(map, "scopes")? {
            let m = value.as_object()?;
            out.scopes.push(Scope {
                id: ScopeId(string(m, "id")?),
                label: string(m, "label")?,
            });
        }
        for value in array(map, "items")? {
            let m = value.as_object()?;
            let mut scopes = Vec::new();
            for value in array(m, "scopes")? {
                scopes.push(ScopeId(value.as_str()?.to_owned()));
            }
            out.items.push(ItemPresentation {
                target: OwnedItem {
                    owner: owner.clone(),
                    item: ItemId(string(m, "item_id")?),
                },
                section: optional(m, "section")?.map(SectionId),
                scopes,
                image: None,
            });
        }
    }
    if typed {
        for value in array(map, "schemas")? {
            let m = value.as_object()?;
            out.schemas.push(ExecutableSchemaBinding {
                executable: ExecutableKey {
                    target: OwnedItem {
                        owner: owner.clone(),
                        item: ItemId(string(m, "item_id")?),
                    },
                    action: optional(m, "action_id")?.map(ActionId),
                },
                schema: Arc::new(schema(m.get("schema")?)?),
            });
        }
    }
    if out.schemas.len() > 4096
        || out.items.len() > 4096
        || out.sections.len() > 128
        || out.scopes.len() > 128
    {
        return None;
    }
    Some(())
}
pub(crate) fn validate(owner: &PluginId, metadata: &ResultPresentation, items: &[Item]) -> Option<()> {
    let targets: BTreeSet<_> = items
        .iter()
        .map(|i| OwnedItem {
            owner: owner.clone(),
            item: i.stable_id.clone(),
        })
        .collect();
    let mut executables = BTreeSet::new();
    for item in items {
        let target = OwnedItem {
            owner: owner.clone(),
            item: item.stable_id.clone(),
        };
        if item.actions.is_empty() {
            executables.insert(ExecutableKey { target, action: None });
        } else {
            for action in &item.actions {
                executables.insert(ExecutableKey {
                    target: target.clone(),
                    action: Some(action.action_id.clone()),
                });
            }
        }
    }
    metadata.validate(owner, &targets, &executables).ok()
}
pub(crate) fn fingerprint(schema: &ArgumentSchema) -> Option<String> {
    use std::fmt::Write;
    let mut hex = String::with_capacity(64);
    for byte in schema.fingerprint().ok()? {
        write!(hex, "{byte:02x}").ok()?;
    }
    Some(hex)
}
pub(crate) fn outcome(frame: &Map<String, Value>, schema: &ArgumentSchema) -> Option<SubmissionOutcome> {
    let result = match frame.get("status")?.as_str()? {
        "completed" => SubmissionOutcome::Completed,
        "failed" => SubmissionOutcome::Failed(match frame.get("code")?.as_str()? {
            "unavailable" => SubmissionFailureCode::Unavailable,
            "denied" => SubmissionFailureCode::Denied,
            "cancelled" => SubmissionFailureCode::Cancelled,
            "timeout" => SubmissionFailureCode::Timeout,
            "unsupported" => SubmissionFailureCode::Unsupported,
            "failed" => SubmissionFailureCode::Failed,
            _ => return None,
        }),
        "field_failures" => {
            let mut failures = Vec::new();
            for value in array(frame, "failures")? {
                let m = value.as_object()?;
                failures.push(SubmissionFieldFailure {
                    field: FieldId(string(m, "field_id")?),
                    code: match m.get("code")?.as_str()? {
                        "required" => SubmissionFieldCode::Required,
                        "too_short" => SubmissionFieldCode::TooShort,
                        "too_long" => SubmissionFieldCode::TooLong,
                        "invalid_selection" => SubmissionFieldCode::InvalidSelection,
                        "rejected" => SubmissionFieldCode::Rejected,
                        _ => return None,
                    },
                });
            }
            SubmissionOutcome::InvalidFields(failures)
        }
        _ => return None,
    };
    result.validate(Some(schema)).ok()?;
    Some(result)
}
