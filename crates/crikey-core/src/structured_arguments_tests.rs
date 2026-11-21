use super::*;
use crate::{ItemId, PluginId};
fn field(name: &str, kind: FieldKind) -> ArgumentField {
    ArgumentField {
        id: FieldId(name.into()),
        label: name.into(),
        required: true,
        placeholder: None,
        validation_message: Some("Check input".into()),
        kind,
    }
}
fn schema() -> ArgumentSchema {
    ArgumentSchema {
        fields: vec![
            field(
                "text",
                FieldKind::Text {
                    min_bytes: 2,
                    max_bytes: MAX_TEXT_BYTES,
                    default: Some("ok".into()),
                    multiline: false,
                },
            ),
            field(
                "password",
                FieldKind::Password {
                    min_bytes: 1,
                    max_bytes: MAX_TEXT_BYTES,
                },
            ),
            field(
                "choice",
                FieldKind::Dropdown {
                    options: vec![DropdownOption {
                        id: OptionId("one".into()),
                        label: "One".into(),
                    }],
                    default: Some(OptionId("one".into())),
                },
            ),
        ],
    }
}
fn values() -> ArgumentValues {
    let mut values = schema().default_values().unwrap();
    values.0.insert(
        FieldId("password".into()),
        StructuredValue::Password(Password::new("secret-unique".into()).unwrap()),
    );
    values
}
#[test]
fn passwords_are_redacted_and_absent_from_snapshots() {
    let values = values();
    assert!(!format!("{values:?}").contains("secret-unique"));
    let snapshot = schema().snapshot_values(&values).unwrap();
    assert_eq!(snapshot.len(), 2);
    assert!(!snapshot.contains_key(&FieldId("password".into())));
    let StructuredValue::Password(password) = &values.0[&FieldId("password".into())] else {
        panic!()
    };
    assert_eq!(password.masked(), "••••••••");
    assert_eq!(password.expose_plaintext(), "secret-unique");
    let args = SubmissionArguments::Structured(values);
    assert!(!format!("{args:?}").contains("secret-unique"));
}

#[test]
fn field_limits_defaults_duplicates_and_utf8_boundaries() {
    let mut s = schema();
    s.fields.extend(vec![s.fields[0].clone(); MAX_FIELDS]);
    assert_eq!(s.validate().unwrap_err().code, ValidationCode::TooMany);
    let mut s = schema();
    s.fields[1].id = s.fields[0].id.clone();
    assert_eq!(s.validate().unwrap_err().code, ValidationCode::DuplicateId);
    let mut s = schema();
    let FieldKind::Dropdown { default, .. } = &mut s.fields[2].kind else {
        panic!()
    };
    *default = Some(OptionId("unknown".into()));
    assert_eq!(s.validate().unwrap_err().code, ValidationCode::InvalidDefault);
    assert!(Password::new("é".repeat(MAX_TEXT_BYTES / 2)).is_ok());
    assert_eq!(
        Password::new("é".repeat(MAX_TEXT_BYTES / 2 + 1)).unwrap_err(),
        ValidationCode::TooLong
    );
    let mut s = schema();
    let FieldKind::Text { max_bytes, .. } = &mut s.fields[0].kind else {
        panic!()
    };
    *max_bytes = MAX_TEXT_BYTES + 1;
    assert_eq!(s.validate().unwrap_err().field, Some(FieldId("text".into())));
}

#[test]
fn values_report_field_codes_not_rejected_content() {
    let mut v = values();
    v.0.insert(
        FieldId("text".into()),
        StructuredValue::NonSecret(NonSecretValue::Text("é".repeat(MAX_TEXT_BYTES / 2 + 1))),
    );
    v.0.insert(
        FieldId("choice".into()),
        StructuredValue::NonSecret(NonSecretValue::Dropdown(OptionId("bad".into()))),
    );
    v.0.insert(
        FieldId("unknown-secret-identity".into()),
        StructuredValue::NonSecret(NonSecretValue::Text("private-rejected".into())),
    );
    let errors = schema().validate_values(&v).unwrap_err();
    assert!(errors
        .iter()
        .any(|e| e.field.as_ref().is_some_and(|f| f.0 == "text") && e.code == ValidationCode::TooLong));
    assert!(errors
        .iter()
        .any(|e| e.field.as_ref().is_some_and(|f| f.0 == "choice")
            && e.code == ValidationCode::InvalidSelection));
    assert!(errors
        .iter()
        .any(|e| e.field.is_none() && e.code == ValidationCode::UnknownField));
    assert!(!format!("{errors:?}").contains("unknown-secret-identity"));
    assert!(!format!("{errors:?}").contains("private-rejected"));
    assert!(schema()
        .validate_values(&ArgumentValues::default())
        .unwrap_err()
        .iter()
        .all(|e| e.code == ValidationCode::Required));
}

#[test]
fn dropdown_option_and_aggregate_schema_limits() {
    let options = (0..MAX_OPTIONS)
        .map(|n| DropdownOption {
            id: OptionId(n.to_string()),
            label: "Option".into(),
        })
        .collect();
    let mut s = ArgumentSchema {
        fields: vec![field(
            "choice",
            FieldKind::Dropdown {
                options,
                default: None,
            },
        )],
    };
    s.validate().unwrap();
    let FieldKind::Dropdown { options, .. } = &mut s.fields[0].kind else {
        panic!()
    };
    options.push(DropdownOption {
        id: OptionId("extra".into()),
        label: String::new(),
    });
    assert_eq!(s.validate().unwrap_err().code, ValidationCode::TooMany);
    let FieldKind::Dropdown { options, .. } = &mut s.fields[0].kind else {
        panic!()
    };
    options.pop();
    for option in options {
        option.label = "a".repeat(MAX_TEXT_BYTES);
    }
    assert_eq!(s.validate().unwrap_err().code, ValidationCode::RetainedBytes);
}

#[test]
fn outcome_validation_has_only_declared_fields_and_fixed_codes() {
    let s = schema();
    let bad = SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
        field: FieldId("unknown".into()),
        code: SubmissionFieldCode::Rejected,
    }]);
    assert_eq!(
        bad.validate(Some(&s)).unwrap_err().code,
        ValidationCode::UnknownField
    );
    let good = SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
        field: FieldId("password".into()),
        code: SubmissionFieldCode::Rejected,
    }]);
    good.validate(Some(&s)).unwrap();
    assert!(good.validate(None).is_err());
}

fn executable(action: Option<&str>) -> ExecutableKey {
    ExecutableKey {
        target: OwnedItem {
            owner: PluginId("owner".into()),
            item: ItemId("item".into()),
        },
        action: action.map(|v| ActionId(v.into())),
    }
}
#[test]
fn actionless_identity_is_real_and_drafts_preserve_exact_target() {
    let none = executable(None);
    let run = executable(Some("run"));
    assert_ne!(none, run);
    none.validate().unwrap();
    assert_eq!(
        executable(Some("")).validate().unwrap_err().code,
        ValidationCode::EmptyId
    );
    let s = schema();
    let draft = s.snapshot_draft(none.clone(), &values()).unwrap();
    assert_eq!(draft.executable, none);
    assert!(!draft.values.contains_key(&FieldId("password".into())));
    s.validate_draft(&draft).unwrap();
    let mut bad = draft;
    bad.values.insert(
        FieldId("password".into()),
        NonSecretValue::Text("forbidden".into()),
    );
    assert_eq!(
        s.validate_draft(&bad).unwrap_err().code,
        ValidationCode::WrongType
    );
}
#[test]
fn secret_edits_are_in_place_utf8_bounded_and_redacted() {
    let mut password = Password::new("aéz".into()).unwrap();
    assert_eq!(
        password.replace_range(2..3, "x"),
        Err(ValidationCode::InvalidBoundary)
    );
    assert_eq!(password.expose_plaintext(), "aéz");
    password.replace_range(1..3, "秘密").unwrap();
    assert_eq!(password.expose_plaintext(), "a秘密z");
    assert_eq!(
        password.replace_range(0..0, &"x".repeat(MAX_TEXT_BYTES)),
        Err(ValidationCode::TooLong)
    );
    assert_eq!(format!("{password:?}"), "Password([REDACTED])");
    password.replace_range(0..8, "").unwrap();
    assert!(password.is_empty());
}
#[test]
fn malformed_schema_is_checked_before_defaults_drafts_and_submission() {
    let mut s = schema();
    s.fields[0].id = FieldId(String::new());
    assert_eq!(s.default_values().unwrap_err().code, ValidationCode::EmptyId);
    assert_eq!(
        s.validate_values(&ArgumentValues::default()).unwrap_err()[0].code,
        ValidationCode::EmptyId
    );
    let draft = NonSecretDraft {
        executable: executable(None),
        values: BTreeMap::new(),
    };
    assert_eq!(
        s.validate_draft(&draft).unwrap_err().code,
        ValidationCode::EmptyId
    );
}
#[test]
fn consumer_fixed_outcomes_reject_duplicates_empty_and_excessive_fields() {
    let s = schema();
    let failure = SubmissionFieldFailure {
        field: FieldId("text".into()),
        code: SubmissionFieldCode::Rejected,
    };
    assert_eq!(
        SubmissionOutcome::InvalidFields(vec![])
            .validate(Some(&s))
            .unwrap_err()
            .code,
        ValidationCode::TooMany
    );
    assert_eq!(
        SubmissionOutcome::InvalidFields(vec![failure.clone(), failure.clone()])
            .validate(Some(&s))
            .unwrap_err()
            .code,
        ValidationCode::DuplicateId
    );
    assert_eq!(
        SubmissionOutcome::InvalidFields(vec![failure; MAX_FIELDS + 1])
            .validate(Some(&s))
            .unwrap_err()
            .code,
        ValidationCode::TooMany
    );
    SubmissionOutcome::Completed.validate(None).unwrap();
    SubmissionOutcome::Failed(SubmissionFailureCode::Unsupported)
        .validate(None)
        .unwrap();
}

#[test]
fn dropdown_drafts_reject_overallocated_matching_option_ids() {
    let s = schema();
    let mut id = String::with_capacity(MAX_ID_BYTES + 1);
    id.push_str("one");
    let mut draft = NonSecretDraft {
        executable: executable(None),
        values: BTreeMap::new(),
    };
    draft
        .values
        .insert(FieldId("choice".into()), NonSecretValue::Dropdown(OptionId(id)));
    assert_eq!(
        s.validate_draft(&draft).unwrap_err().code,
        ValidationCode::TooLong
    );
    let mut live = ArgumentValues::default();
    for (field, value) in draft.values {
        live.0.insert(field, StructuredValue::NonSecret(value));
    }
    assert!(s
        .validate_values(&live)
        .unwrap_err()
        .iter()
        .any(|e| e.code == ValidationCode::TooLong));
}

#[test]
fn secret_middle_edits_preserve_suffix_and_existing_allocation() {
    let mut storage = String::with_capacity(MAX_TEXT_BYTES);
    storage.push_str("aéz");
    let mut password = Password::new(storage).unwrap();
    let capacity = password.retained_bytes();
    password.replace_range(1..3, "秘密").unwrap();
    assert_eq!(password.expose_plaintext(), "a秘密z");
    password.replace_range(1..7, "").unwrap();
    assert_eq!(password.expose_plaintext(), "az");
    password.replace_range(1..1, "é").unwrap();
    assert_eq!(password.expose_plaintext(), "aéz");
    assert_eq!(password.retained_bytes(), capacity);
    assert_eq!(
        password.replace_range(0..usize::MAX, ""),
        Err(ValidationCode::InvalidBoundary)
    );
}

fn unhex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => panic!("invalid golden hex"),
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect()
}

fn golden_mixed_schema() -> ArgumentSchema {
    ArgumentSchema {
        fields: vec![
            ArgumentField {
                id: FieldId("t".into()),
                label: "é".into(),
                required: true,
                placeholder: Some(String::new()),
                validation_message: Some("Use text".into()),
                kind: FieldKind::Text {
                    min_bytes: 0,
                    max_bytes: 8,
                    multiline: true,
                    default: Some("x".into()),
                },
            },
            ArgumentField {
                id: FieldId("p".into()),
                label: "Password".into(),
                required: true,
                placeholder: None,
                validation_message: None,
                kind: FieldKind::Password {
                    min_bytes: 1,
                    max_bytes: MAX_TEXT_BYTES,
                },
            },
            ArgumentField {
                id: FieldId("d".into()),
                label: "Choice".into(),
                required: false,
                placeholder: None,
                validation_message: Some(String::new()),
                kind: FieldKind::Dropdown {
                    options: vec![
                        DropdownOption {
                            id: OptionId("a".into()),
                            label: "Alpha".into(),
                        },
                        DropdownOption {
                            id: OptionId("β".into()),
                            label: "Beta".into(),
                        },
                    ],
                    default: Some(OptionId("β".into())),
                },
            },
        ],
    }
}

#[test]
fn fingerprint_v1_exact_canonical_vectors() {
    let minimal = ArgumentSchema {
        fields: vec![ArgumentField {
            id: FieldId("text".into()),
            label: "Text".into(),
            required: false,
            placeholder: None,
            validation_message: None,
            kind: FieldKind::Text {
                min_bytes: 0,
                max_bytes: MAX_TEXT_BYTES,
                multiline: false,
                default: None,
            },
        }],
    };
    // These exact canonical bytes/digests are shared with the Python SDK.
    let vectors = [
        (ArgumentSchema::default(),
         "6372696b65792e617267756d656e742d736368656d61000100000000000000",
         "3265b557c583dda06d75f83cdcf9d5e898c972b0772614f475819ffe4edd22bf"),
        (minimal,
         "6372696b65792e617267756d656e742d736368656d61000100000001000000040000007465787404000000546578740000000000000000002000000000",
         "26f7c14f86657e87ae8cc9279ad7707244d827bb6c202e4638de9fbf22ba763c"),
        (golden_mixed_schema(),
         concat!("6372696b65792e617267756d656e742d736368656d61000100000003000000",
                 "010000007402000000c3a90101000000000108000000557365207465787400000000000800000001010100000078",
                 "01000000700800000050617373776f7264010000010100000000200000",
                 "01000000640600000043686f696365000001000000000202000000010000006105000000416c70686102000000ceb204000000426574610102000000ceb2"),
         "74e577d66d4bed1332aacdcbc1b692c8bf736384edb9cc8d91d303a529baac2e"),
    ];
    for (schema, canonical, digest) in vectors {
        let expected = unhex(digest);
        let encoded: [u8; 32] = Sha256::digest(unhex(canonical)).into();
        assert_eq!(encoded.as_slice(), expected);
        assert_eq!(schema.fingerprint().unwrap().as_slice(), expected);
    }
}

#[test]
fn fingerprint_covers_every_property_and_order() {
    let original = golden_mixed_schema();
    let expected = original.fingerprint().unwrap();
    let mutations: &[fn(&mut ArgumentSchema)] = &[
        |s| s.fields[0].id.0.push('x'),
        |s| s.fields[0].label.push('x'),
        |s| s.fields[0].required = false,
        |s| s.fields[0].placeholder = None,
        |s| s.fields[0].placeholder = Some("x".into()),
        |s| s.fields[0].validation_message = None,
        |s| s.fields[0].validation_message = Some(String::new()),
        |s| {
            if let FieldKind::Text { max_bytes, .. } = &mut s.fields[0].kind {
                *max_bytes = 9;
            }
        },
        |s| {
            if let FieldKind::Text { multiline, .. } = &mut s.fields[0].kind {
                *multiline = false;
            }
        },
        |s| {
            if let FieldKind::Text { default, .. } = &mut s.fields[0].kind {
                *default = None;
            }
        },
        |s| {
            if let FieldKind::Password { min_bytes, .. } = &mut s.fields[1].kind {
                *min_bytes = 2;
            }
        },
        |s| {
            if let FieldKind::Password { max_bytes, .. } = &mut s.fields[1].kind {
                *max_bytes = 16;
            }
        },
        |s| {
            if let FieldKind::Dropdown { options, .. } = &mut s.fields[2].kind {
                options[0].id.0.push('x');
            }
        },
        |s| {
            if let FieldKind::Dropdown { options, .. } = &mut s.fields[2].kind {
                options[0].label.push('x');
            }
        },
        |s| {
            if let FieldKind::Dropdown { options, .. } = &mut s.fields[2].kind {
                options.swap(0, 1);
            }
        },
        |s| {
            if let FieldKind::Dropdown { default, .. } = &mut s.fields[2].kind {
                *default = None;
            }
        },
        |s| s.fields.swap(0, 1),
        |s| {
            s.fields[0].kind = FieldKind::Password {
                min_bytes: 0,
                max_bytes: 8,
            }
        },
    ];
    for mutate in mutations {
        let mut changed = original.clone();
        mutate(&mut changed);
        assert_ne!(changed.fingerprint().unwrap(), expected);
    }
    let mut changed = original.clone();
    changed.fields[0].kind = FieldKind::Text {
        min_bytes: 1,
        max_bytes: 8,
        multiline: true,
        default: Some("x".into()),
    };
    let before = changed.fingerprint().unwrap();
    if let FieldKind::Text { min_bytes, .. } = &mut changed.fields[0].kind {
        *min_bytes = 0;
    }
    assert_ne!(changed.fingerprint().unwrap(), before);
}

#[test]
fn fingerprint_refuses_invalid_bounds_and_counts_before_narrowing() {
    let mut invalid = golden_mixed_schema();
    invalid.fields[1].kind = FieldKind::Password {
        min_bytes: 0,
        max_bytes: usize::MAX,
    };
    assert_eq!(
        invalid.fingerprint().unwrap_err().code,
        ValidationCode::InvalidBounds
    );
    let mut invalid = golden_mixed_schema();
    invalid.fields = vec![invalid.fields[0].clone(); MAX_FIELDS + 1];
    assert_eq!(invalid.fingerprint().unwrap_err().code, ValidationCode::TooMany);
    let mut invalid = golden_mixed_schema();
    if let FieldKind::Dropdown { options, .. } = &mut invalid.fields[2].kind {
        options.resize(MAX_OPTIONS + 1, options[0].clone());
    }
    assert_eq!(invalid.fingerprint().unwrap_err().code, ValidationCode::TooMany);
}

#[test]
fn fingerprint_distinguishes_absent_and_present_empty_defaults() {
    let mut schema = golden_mixed_schema();
    schema.fields[0].required = false;
    if let FieldKind::Text { default, .. } = &mut schema.fields[0].kind {
        *default = None;
    }
    let absent = schema.fingerprint().unwrap();
    if let FieldKind::Text { default, .. } = &mut schema.fields[0].kind {
        *default = Some(String::new());
    }
    assert_ne!(schema.fingerprint().unwrap(), absent);
}
