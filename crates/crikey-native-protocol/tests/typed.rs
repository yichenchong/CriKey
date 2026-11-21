use crikey_core::structured_arguments as a;
use crikey_core::{ItemId, PluginId};
use crikey_native_protocol::{message as m, typed, wire, Message, ProtocolError};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

fn nested(tag: u32, bytes: &[u8], out: &mut Vec<u8>) {
    wire::encode_key(tag, wire::WireType::Length, out);
    wire::encode_varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}
fn text_field(id: &str) -> m::ArgumentField {
    m::ArgumentField {
        id: id.into(),
        label: "Text".into(),
        kind: 1,
        ..Default::default()
    }
}
fn schema() -> a::ArgumentSchema {
    typed::from_schema(m::ArgumentSchema {
        fields: vec![text_field("text")],
        ..Default::default()
    })
    .unwrap()
}
fn values() -> a::ArgumentValues {
    a::ArgumentValues(BTreeMap::from([
        (
            a::FieldId("text".into()),
            a::StructuredValue::NonSecret(a::NonSecretValue::Text("private text".into())),
        ),
        (
            a::FieldId("password".into()),
            a::StructuredValue::Password(a::Password::new("private password".into()).unwrap()),
        ),
        (
            a::FieldId("choice".into()),
            a::StructuredValue::NonSecret(a::NonSecretValue::Dropdown(a::OptionId("a".into()))),
        ),
    ]))
}

#[test]
fn fingerprints_normalize_omissions_but_not_presence_or_unknowns() {
    let wire = m::ArgumentSchema {
        fields: vec![text_field("text")],
        ..Default::default()
    };
    let normalized = typed::from_schema(wire.clone()).unwrap();
    let digest = normalized.fingerprint().unwrap();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "26f7c14f86657e87ae8cc9279ad7707244d827bb6c202e4638de9fbf22ba763c"
    );
    let mut explicit = wire.clone();
    explicit.fields[0].max_bytes = Some(8192);
    explicit.unknown.push_raw(&[0xa0, 0x06, 0x01]);
    assert_eq!(
        typed::from_schema(explicit).unwrap().fingerprint().unwrap(),
        digest
    );
    let mut present = wire;
    present.fields[0].placeholder = Some(String::new());
    assert_ne!(
        typed::from_schema(present).unwrap().fingerprint().unwrap(),
        digest
    );
    let round_trip = typed::from_schema(
        m::ArgumentSchema::decode(&typed::to_schema(&normalized).unwrap().encode()).unwrap(),
    )
    .unwrap();
    assert_eq!(round_trip, normalized);
}

#[test]
fn typed_request_preserves_actionless_presence_and_redacts_all_values() {
    let request = typed::to_request(&ItemId("item".into()), None, [7; 32], values()).unwrap();
    let debug = format!("{request:?}");
    assert!(!debug.contains("private text"));
    assert!(!debug.contains("private password"));
    let envelope = m::Envelope {
        payload: Some(m::Payload::TypedExecuteRequest(request)),
        ..Default::default()
    };
    let bytes = m::checked_encode(&envelope).unwrap();
    assert_eq!(&bytes[..2], &[0xfa, 0x02]);
    let decoded = m::Envelope::decode(&bytes).unwrap();
    let Some(m::Payload::TypedExecuteRequest(request)) = decoded.payload else {
        panic!("typed request")
    };
    assert!(request.action_id.is_none());
    assert_eq!(request.values.0.len(), 3);
    let actionful = typed::to_request(
        &ItemId("item".into()),
        Some(&crikey_core::ActionId("go".into())),
        [7; 32],
        a::ArgumentValues::default(),
    )
    .unwrap();
    assert_eq!(
        m::TypedExecuteRequest::decode(&actionful.encode())
            .unwrap()
            .action_id
            .as_deref(),
        Some("go")
    );
    assert!(typed::to_request(
        &ItemId("item".into()),
        Some(&crikey_core::ActionId(String::new())),
        [7; 32],
        a::ArgumentValues::default()
    )
    .is_err());
}

#[test]
fn typed_value_unknown_fields_round_trip_without_callback_materialization() {
    let mut value = Vec::new();
    nested(1, b"text", &mut value);
    wire::encode_key(2, wire::WireType::Varint, &mut value);
    wire::encode_varint(1, &mut value);
    nested(3, b"private text", &mut value);
    nested(100, b"opaque", &mut value);
    let mut request = Vec::new();
    nested(1, b"item", &mut request);
    nested(3, &[9; 32], &mut request);
    nested(4, &value, &mut request);
    let decoded = m::TypedExecuteRequest::decode(&request).unwrap();
    assert_eq!(decoded.values.0.len(), 1);
    assert_eq!(decoded.encode(), request);
}

#[test]
fn duplicate_known_members_and_values_are_refused_across_fragments() {
    let request = typed::to_request(
        &ItemId("item".into()),
        None,
        [7; 32],
        a::ArgumentValues::default(),
    )
    .unwrap();
    let mut envelope = Vec::new();
    nested(47, &request.encode(), &mut envelope);
    nested(47, &request.encode(), &mut envelope);
    assert!(m::Envelope::decode(&envelope).is_err());
    let mut value = Vec::new();
    nested(1, b"x", &mut value);
    wire::encode_key(2, wire::WireType::Varint, &mut value);
    wire::encode_varint(1, &mut value);
    nested(3, b"v", &mut value);
    let mut bytes = request.encode();
    nested(4, &value, &mut bytes);
    nested(4, &value, &mut bytes);
    assert!(m::TypedExecuteRequest::decode(&bytes).is_err());
    let mut field = text_field("x").encode();
    nested(1, b"x", &mut field);
    assert!(m::ArgumentField::decode(&field).is_err());
}

#[test]
fn password_defaults_and_hostile_counts_are_refused_on_both_sides() {
    let mut password = text_field("password");
    password.kind = 2;
    password.default_value = Some(String::new());
    assert!(m::ArgumentField::decode(&password.encode()).is_err());
    let wire = m::ArgumentSchema {
        fields: vec![password],
        ..Default::default()
    };
    assert!(typed::from_schema(wire).is_err());
    let excessive = m::ArgumentSchema {
        fields: (0..65).map(|i| text_field(&format!("f{i}"))).collect(),
        ..Default::default()
    };
    assert!(m::ArgumentSchema::decode(&excessive.encode()).is_err());
    let mut presentation = m::ResultPresentation::default();
    presentation.schemas.push(m::ExecutableSchemaBinding {
        item_id: "item".into(),
        schema: Some(excessive),
        ..Default::default()
    });
    let frame = m::Envelope {
        payload: Some(m::Payload::CatalogBatch(m::CatalogBatch {
            presentation: Some(presentation),
            ..Default::default()
        })),
        ..Default::default()
    };
    assert!(m::validate_outbound(&frame).is_err());
}

#[test]
fn allocation_and_unknown_budgets_apply_before_schema_materialization() {
    let options: Vec<_> = (0..256)
        .map(|i| m::DropdownOption {
            id: format!("o{i}"),
            label: "Choice".into(),
            ..Default::default()
        })
        .collect();
    let fields = (0..16)
        .map(|i| m::ArgumentField {
            id: format!("f{i}"),
            label: "Choice".into(),
            kind: 3,
            options: options.clone(),
            ..Default::default()
        })
        .collect();
    let large = m::ArgumentSchema {
        fields,
        ..Default::default()
    };
    let bytes = large.encode();
    assert!(bytes.len() < 262144);
    assert!(matches!(
        m::ArgumentSchema::decode(&bytes),
        Err(ProtocolError::DecodeBudgetExceeded { .. })
    ));
    let mut unknown = Vec::new();
    nested(100, &vec![0; 140000], &mut unknown);
    nested(101, b"x", &mut unknown);
    assert!(matches!(
        m::ArgumentSchema::decode(&unknown),
        Err(ProtocolError::DecodeBudgetExceeded { .. })
    ));
}

#[test]
fn schema_field_limits_accumulate_through_nested_binding_fragments() {
    let first = m::ArgumentSchema {
        fields: (0..32).map(|i| text_field(&format!("f{i}"))).collect(),
        ..Default::default()
    };
    let second = m::ArgumentSchema {
        fields: (32..65).map(|i| text_field(&format!("f{i}"))).collect(),
        ..Default::default()
    };
    let mut binding = Vec::new();
    nested(1, b"item", &mut binding);
    nested(3, &first.encode(), &mut binding);
    nested(3, &second.encode(), &mut binding);
    assert!(m::ExecutableSchemaBinding::decode(&binding).is_err());
}

#[test]
fn presentation_ownership_and_references_are_admitted_as_one_transaction() {
    let owner = PluginId("owner".into());
    let presentation = m::ResultPresentation {
        items: vec![m::ItemPresentation {
            item_id: "item".into(),
            section_id: Some("undeclared".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let presentation = typed::from_presentation(presentation, &owner).unwrap();
    let target = a::OwnedItem {
        owner: owner.clone(),
        item: ItemId("item".into()),
    };
    assert!(presentation
        .validate(&owner, &BTreeSet::from([target]), &BTreeSet::new())
        .is_err());
}

#[test]
fn field_failures_are_only_unique_current_ids_and_fixed_codes() {
    let schema = schema();
    let outcome = a::SubmissionOutcome::InvalidFields(vec![a::SubmissionFieldFailure {
        field: a::FieldId("text".into()),
        code: a::SubmissionFieldCode::Rejected,
    }]);
    assert_eq!(
        typed::from_response(
            typed::to_response(outcome.clone().into(), &schema).unwrap(),
            &schema
        )
        .unwrap(),
        outcome.into()
    );
    let response = m::TypedExecuteResponse {
        status: 2,
        failures: vec![m::TypedFieldFailure {
            field_id: "unknown-private-data".into(),
            code: 5,
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(typed::from_response(response, &schema).is_err());
    let response = m::TypedExecuteResponse {
        status: 3,
        failure_code: 999,
        ..Default::default()
    };
    assert!(typed::from_response(response, &schema).is_err());
    for code in [
        a::SubmissionFailureCode::Unavailable,
        a::SubmissionFailureCode::Denied,
        a::SubmissionFailureCode::Cancelled,
        a::SubmissionFailureCode::Timeout,
        a::SubmissionFailureCode::Unsupported,
        a::SubmissionFailureCode::Failed,
        a::SubmissionFailureCode::SchemaMismatch,
        a::SubmissionFailureCode::ResourceLimit,
        a::SubmissionFailureCode::ProtocolViolation,
    ] {
        let outcome = a::SubmissionOutcome::Failed(code);
        assert_eq!(
            typed::from_response(
                typed::to_response(outcome.clone().into(), &schema).unwrap(),
                &schema
            )
            .unwrap(),
            outcome.into()
        );
    }
}

#[test]
fn sensitive_decode_discards_diagnostics_but_keeps_fixed_correlation() {
    let mut error = m::StructuredError {
        code: m::ErrorCode::Plugin,
        message: "private password".into(),
        detail: "private text".into(),
        request_id: 42,
        ..Default::default()
    };
    error.unknown.push_raw(&[0xa0, 0x06, 0x01]);
    let frame = m::Envelope {
        request_id: 42,
        payload: Some(m::Payload::Error(error)),
        ..Default::default()
    };
    let bytes = frame.encode();
    let normal = m::Envelope::decode_with_diagnostics(&bytes, &AtomicBool::new(false)).unwrap();
    assert!(format!("{normal:?}").contains("private password"));
    let sensitive = m::Envelope::decode_with_diagnostics(&bytes, &AtomicBool::new(true)).unwrap();
    let Some(m::Payload::Error(error)) = sensitive.payload else {
        panic!("fixed error")
    };
    assert_eq!(error.code, m::ErrorCode::Plugin);
    assert_eq!(error.request_id, 42);
    assert!(error.message.is_empty());
    assert!(error.detail.is_empty());
    assert!(error.unknown.as_bytes().is_empty());
}

#[test]
fn generated_proto_empty_text_omission_normalizes_without_legacy_arguments() {
    let mut value = Vec::new();
    nested(1, b"text", &mut value);
    wire::encode_key(2, wire::WireType::Varint, &mut value);
    wire::encode_varint(1, &mut value);
    let mut request = Vec::new();
    nested(1, b"item", &mut request);
    nested(3, &[1; 32], &mut request);
    nested(4, &value, &mut request);
    let decoded = m::TypedExecuteRequest::decode(&request).unwrap();
    assert!(
        matches!(decoded.values.0.get(&a::FieldId("text".into())),Some(a::StructuredValue::NonSecret(a::NonSecretValue::Text(value))) if value.is_empty())
    );
}

#[test]
fn additive_sidecar_scope_and_response_tags_are_frozen() {
    let catalog = m::CatalogBatch {
        presentation: Some(m::ResultPresentation::default()),
        ..Default::default()
    }
    .encode();
    let result = m::ResultBatch {
        presentation: Some(m::ResultPresentation::default()),
        ..Default::default()
    }
    .encode();
    let query = m::SuggestRequest {
        selected_scope_id: Some("scope".into()),
        ..Default::default()
    }
    .encode();
    let response = m::Envelope {
        payload: Some(m::Payload::TypedExecuteResponse(m::TypedExecuteResponse {
            status: 1,
            ..Default::default()
        })),
        ..Default::default()
    }
    .encode();
    assert_eq!(catalog, vec![0x2a, 0]);
    assert_eq!(result, vec![0x2a, 0]);
    assert_eq!(query[0], 0x32);
    assert_eq!(&response[..2], &[0x82, 0x03]);
}

#[test]
fn producer_preflight_combines_rows_and_sidecar_allocation() {
    let candidates: Vec<_> = (0..1024)
        .map(|index| m::Item {
            stable_id: format!("item-{index}"),
            label: "x".repeat(8192),
            ..Default::default()
        })
        .collect();
    let count = m::max_decodable_items(&candidates);
    assert!(count > 0 && count < candidates.len());
    let items: Vec<_> = candidates.into_iter().take(count).collect();
    let presentation = m::ResultPresentation {
        sections: (0..128)
            .map(|index| m::PresentationDeclaration {
                id: format!("section-{index}"),
                label: "s".repeat(8192),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    typed::check_presentation(&presentation).unwrap();
    assert!(typed::presentation_charge(&presentation) > m::BATCH_OVERHEAD_RESERVE);
    let mut envelope = m::Envelope {
        payload: Some(m::Payload::CatalogBatch(m::CatalogBatch {
            items,
            done: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    m::validate_outbound(&envelope).unwrap();
    let Some(m::Payload::CatalogBatch(batch)) = &mut envelope.payload else {
        panic!("catalog");
    };
    batch.presentation = Some(presentation);
    assert!(matches!(
        m::validate_outbound(&envelope),
        Err(ProtocolError::DecodeBudgetExceeded { .. })
    ));
}

#[test]
fn legacy_execute_preflight_preserves_the_exact_frame_allowance() {
    // Four-byte lengths, a one-byte argument tag and the two-byte execute tag
    // account for eleven bytes around this ordinary legacy argument.
    let mut envelope = m::Envelope {
        payload: Some(m::Payload::Execute(m::ExecuteRequest {
            argument: "x".repeat(crikey_native_protocol::MAX_FRAME_BYTES - 11),
            ..Default::default()
        })),
        ..Default::default()
    };
    m::validate_outbound(&envelope).unwrap();
    let Some(m::Payload::Execute(request)) = &mut envelope.payload else {
        panic!("legacy execute");
    };
    request.argument.push('x');
    assert!(matches!(
        m::validate_outbound(&envelope),
        Err(ProtocolError::FrameTooLarge(_))
    ));
}

#[test]
fn typed_navigation_round_trips_without_submission_status_or_field_results() {
    use crikey_core::structured_view::{RouteId, StructuredRouteOutcome, TypedExecutionOutcome};
    for route in [
        StructuredRouteOutcome::Push(RouteId("grid".into())),
        StructuredRouteOutcome::Replace(RouteId("form".into())),
    ] {
        let outcome = TypedExecutionOutcome::Navigate(route);
        let response = typed::to_response(outcome.clone(), &schema()).unwrap();
        assert!(response.navigation.is_some());
        assert_eq!(response.status, 0);
        assert_eq!(response.failure_code, 0);
        assert!(response.failures.is_empty());
        assert_eq!(
            typed::from_response(response.clone(), &schema()).unwrap(),
            outcome
        );
        let mut mixed = response.clone();
        mixed.status = 1;
        assert!(typed::from_response(mixed, &schema()).is_err());
        let mut mixed = response.clone();
        mixed.failure_code = 1;
        assert!(typed::from_response(mixed, &schema()).is_err());
        let mut mixed = response;
        mixed.failures.push(m::TypedFieldFailure {
            field_id: "text".into(),
            code: 5,
            ..Default::default()
        });
        assert!(typed::from_response(mixed, &schema()).is_err());
    }
}
