//! Red-first coverage for the hand-written native protocol codec (spec 16.3).

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, PluginId,
};
use crikey_native_protocol::{message, wire, Endpoint, Message, ProtocolError};

fn unknown() -> wire::UnknownFields {
    wire::UnknownFields::default()
}

fn structured_error() -> message::StructuredError {
    message::StructuredError {
        code: message::ErrorCode::from_i32(1),
        message: "plugin failed".to_owned(),
        detail: "a detailed failure".to_owned(),
        request_id: 41,
        unknown: unknown(),
    }
}

fn action() -> message::Action {
    message::Action {
        action_id: "open".to_owned(),
        label: "Open".to_owned(),
        description: "Open the selected item".to_owned(),
        icon_reference: "icon-open".to_owned(),
        execution_policy: "plugin".to_owned(),
        // Non-default so the all-fields round-trip actually covers tag 6.
        applicable_categories: vec!["application".to_owned(), "documents".to_owned()],
        unknown: unknown(),
    }
}

fn item() -> message::Item {
    message::Item {
        stable_id: "plugin:item:1".to_owned(),
        label: "Example item".to_owned(),
        description: "An item used by the wire tests".to_owned(),
        target: "/tmp/example".to_owned(),
        category: "application".to_owned(),
        search_terms: vec!["example".to_owned(), "demo".to_owned()],
        icon_reference: "icon-example".to_owned(),
        score_hint: -17,
        metadata: BTreeMap::from([
            ("z-last".to_owned(), "value-z".to_owned()),
            ("a-first".to_owned(), "value-a".to_owned()),
        ]),
        actions: vec![action()],
        unknown: unknown(),
    }
}

fn handshake() -> message::Handshake {
    message::Handshake {
        protocol_version: 1,
        plugin_id: "dev.example.native".to_owned(),
        plugin_version: "2.3.4".to_owned(),
        capabilities: vec!["streaming_suggestions".to_owned(), "cancellation".to_owned()],
        session_token: "0123456789abcdef0123456789abcdef".to_owned(),
        plugin_name: "Example Native".to_owned(),
        sdk_version: "1.0.0".to_owned(),
        unknown: unknown(),
    }
}

fn handshake_ack() -> message::HandshakeAck {
    message::HandshakeAck {
        protocol_version: 1,
        host_capabilities: vec!["streaming_catalog".to_owned(), "events".to_owned()],
        host_version: "9.8.7".to_owned(),
        accepted: true,
        reject_reason: "accepted".to_owned(),
        max_frame_bytes: 8 * 1024 * 1024,
        initial_credits: 8,
        unknown: unknown(),
    }
}

fn assert_round_trip<M: Message>(value: M) {
    let encoded = value.encode();
    let decoded = M::decode(&encoded).expect("encoded message must decode");
    assert_eq!(decoded.encode(), encoded, "re-encoding changed the message");
}

fn assert_default_encoding<M: Message>(value: M) {
    assert!(value.encode().is_empty(), "default message emitted bytes");
    let decoded = M::decode(&[]).expect("empty proto3 message is valid");
    assert!(decoded.encode().is_empty(), "empty decode was not default");
}

#[test]
fn varints_round_trip_and_truncated_input_is_malformed() {
    for value in [0, 1, 127, 128, u64::MAX] {
        let mut encoded = Vec::new();
        wire::encode_varint(value, &mut encoded);
        let mut cursor = 0;
        let decoded = wire::decode_varint(&encoded, &mut cursor);
        assert!(matches!(decoded, Ok(decoded_value) if decoded_value == value));
        assert_eq!(cursor, encoded.len());
    }

    for bytes in [
        &[0x80_u8][..],
        &[0x80_u8, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80][..],
    ] {
        let mut cursor = 0;
        let result = catch_unwind(AssertUnwindSafe(|| wire::decode_varint(bytes, &mut cursor)));
        assert!(matches!(result, Ok(Err(ProtocolError::Malformed(_)))));
    }
}

#[test]
fn zigzag_values_round_trip() {
    for value in [i64::MIN, -1, 0, 1, i64::MAX] {
        assert_eq!(wire::zigzag_decode(wire::zigzag_encode(value)), value);
    }
}

#[test]
fn keys_pin_field_number_and_wire_type_bits() {
    let cases = [
        (wire::WireType::Varint, vec![0x18]),
        (wire::WireType::Fixed64, vec![0x19]),
        (wire::WireType::Length, vec![0x1a]),
        (wire::WireType::Fixed32, vec![0x1d]),
    ];
    for (wire_type, expected) in cases {
        let mut encoded = Vec::new();
        wire::encode_key(3, wire_type, &mut encoded);
        assert_eq!(encoded, expected);
    }
}

#[test]
fn unknown_enum_values_are_safe_unspecified_values() {
    assert_eq!(message::LifecycleKind::from_i32(i32::MAX).as_i32(), 0);
    assert_eq!(message::BatchState::from_i32(i32::MAX).as_i32(), 0);
    assert_eq!(message::ExecuteOutcomeCode::from_i32(i32::MAX).as_i32(), 0);
    assert_eq!(message::EventKind::from_i32(i32::MAX).as_i32(), 0);
    assert_eq!(message::ResourceKind::from_i32(i32::MAX).as_i32(), 0);
    assert_eq!(message::LogLevel::from_i32(i32::MAX).as_i32(), 0);
    assert_eq!(message::ErrorCode::from_i32(i32::MAX).as_i32(), 0);
}

#[test]
fn proto3_defaults_elide_and_decode_from_empty() {
    assert_default_encoding(message::Envelope {
        connection_id: 0,
        request_id: 0,
        generation: 0,
        deadline_ms: 0,
        payload: None,
        unknown: unknown(),
    });

    assert_default_encoding(message::Handshake {
        protocol_version: 0,
        plugin_id: String::new(),
        plugin_version: String::new(),
        capabilities: Vec::new(),
        session_token: String::new(),
        plugin_name: String::new(),
        sdk_version: String::new(),
        unknown: unknown(),
    });
    assert_default_encoding(message::HandshakeAck {
        protocol_version: 0,
        host_capabilities: Vec::new(),
        host_version: String::new(),
        accepted: false,
        reject_reason: String::new(),
        max_frame_bytes: 0,
        initial_credits: 0,
        unknown: unknown(),
    });
    assert_default_encoding(message::Lifecycle {
        kind: message::LifecycleKind::from_i32(0),
        unknown: unknown(),
    });
    assert_default_encoding(message::LifecycleAck {
        kind: message::LifecycleKind::from_i32(0),
        ok: false,
        error: None,
        unknown: unknown(),
    });
    assert_default_encoding(message::CatalogRequest {
        max_items: 0,
        unknown: unknown(),
    });
    assert_default_encoding(message::CatalogBatch {
        items: Vec::new(),
        done: false,
        sequence: 0,
        error: None,
        unknown: unknown(),
    });
    assert_default_encoding(message::SuggestRequest {
        text: String::new(),
        normalized_text: String::new(),
        selected_item_id: String::new(),
        max_items: 0,
        max_batches: 0,
        unknown: unknown(),
    });
    assert_default_encoding(message::Action {
        action_id: String::new(),
        label: String::new(),
        description: String::new(),
        icon_reference: String::new(),
        execution_policy: String::new(),
        applicable_categories: Vec::new(),
        unknown: unknown(),
    });
    assert_default_encoding(message::Item {
        stable_id: String::new(),
        label: String::new(),
        description: String::new(),
        target: String::new(),
        category: String::new(),
        search_terms: Vec::new(),
        icon_reference: String::new(),
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
        unknown: unknown(),
    });
    assert_default_encoding(message::ResultBatch {
        state: message::BatchState::from_i32(0),
        items: Vec::new(),
        sequence: 0,
        error: None,
        unknown: unknown(),
    });
    assert_default_encoding(message::Cancel {
        reason: String::new(),
        unknown: unknown(),
    });
    assert_default_encoding(message::ExecuteRequest {
        item_id: String::new(),
        action_id: String::new(),
        argument: String::new(),
        unknown: unknown(),
    });
    assert_default_encoding(message::ExecuteResult {
        outcome: message::ExecuteOutcomeCode::from_i32(0),
        error: None,
        unknown: unknown(),
    });
    assert_default_encoding(message::ConfigurationChange {
        values: BTreeMap::new(),
        complete: false,
        unknown: unknown(),
    });
    assert_default_encoding(message::Event {
        kind: message::EventKind::from_i32(0),
        attributes: BTreeMap::new(),
        flags: 0,
        unknown: unknown(),
    });
    assert_default_encoding(message::ResourceRequest {
        kind: message::ResourceKind::from_i32(0),
        reference: String::new(),
        unknown: unknown(),
    });
    assert_default_encoding(message::ResourceResponse {
        reference: String::new(),
        found: false,
        content: Vec::new(),
        media_type: String::new(),
        error: None,
        unknown: unknown(),
    });
    assert_default_encoding(message::LogRecord {
        level: message::LogLevel::from_i32(0),
        message: String::new(),
        timestamp_ms: 0,
        unknown: unknown(),
    });
    assert_default_encoding(message::HealthCheck {
        nonce: 0,
        unknown: unknown(),
    });
    assert_default_encoding(message::HealthReport {
        nonce: 0,
        healthy: false,
        memory_bytes: 0,
        queue_depth: 0,
        in_flight: 0,
        detail: String::new(),
        unknown: unknown(),
    });
    assert_default_encoding(message::StructuredError {
        code: message::ErrorCode::from_i32(0),
        message: String::new(),
        detail: String::new(),
        request_id: 0,
        unknown: unknown(),
    });
    assert_default_encoding(message::FlowControl {
        credits: 0,
        paused: false,
        unknown: unknown(),
    });
    assert_default_encoding(message::Shutdown {
        immediate: false,
        unknown: unknown(),
    });
}

#[test]
fn every_message_round_trips_all_non_default_fields() {
    let error = structured_error();
    let nested_item = item();
    let nested_item_wire = nested_item.encode();
    let a_position = nested_item_wire
        .windows(b"a-first".len())
        .position(|window| window == &b"a-first"[..])
        .expect("first metadata key was not encoded");
    let z_position = nested_item_wire
        .windows(b"z-last".len())
        .position(|window| window == &b"z-last"[..])
        .expect("last metadata key was not encoded");
    assert!(
        a_position < z_position,
        "metadata entries were not encoded in order"
    );
    let nested_item_decoded =
        message::Item::decode(&nested_item_wire).expect("item with maps and actions must decode");
    assert_eq!(nested_item_decoded.metadata, nested_item.metadata);
    assert_eq!(nested_item_decoded.actions.len(), nested_item.actions.len());

    assert_round_trip(message::Envelope {
        connection_id: 7,
        request_id: 8,
        generation: 9,
        deadline_ms: 10,
        payload: Some(message::Payload::Handshake(handshake())),
        unknown: unknown(),
    });
    assert_round_trip(handshake());
    assert_round_trip(handshake_ack());
    assert_round_trip(message::Lifecycle {
        kind: message::LifecycleKind::from_i32(1),
        unknown: unknown(),
    });
    assert_round_trip(message::LifecycleAck {
        kind: message::LifecycleKind::from_i32(2),
        ok: true,
        error: Some(error.clone()),
        unknown: unknown(),
    });
    assert_round_trip(message::CatalogRequest {
        max_items: 500,
        unknown: unknown(),
    });
    assert_round_trip(message::CatalogBatch {
        items: vec![nested_item.clone()],
        done: true,
        sequence: 2,
        error: Some(error.clone()),
        unknown: unknown(),
    });
    assert_round_trip(message::SuggestRequest {
        text: "hello".to_owned(),
        normalized_text: "hello".to_owned(),
        selected_item_id: "plugin:item:1".to_owned(),
        max_items: 100,
        max_batches: 4,
        unknown: unknown(),
    });
    assert_round_trip(action());
    assert_round_trip(nested_item.clone());
    assert_round_trip(message::ResultBatch {
        state: message::BatchState::from_i32(1),
        items: vec![nested_item],
        sequence: 3,
        error: Some(error.clone()),
        unknown: unknown(),
    });
    assert_round_trip(message::Cancel {
        reason: "new query superseded this request".to_owned(),
        unknown: unknown(),
    });
    assert_round_trip(message::ExecuteRequest {
        item_id: "plugin:item:1".to_owned(),
        action_id: "open".to_owned(),
        argument: "--verbose".to_owned(),
        unknown: unknown(),
    });
    assert_round_trip(message::ExecuteResult {
        outcome: message::ExecuteOutcomeCode::from_i32(1),
        error: Some(error.clone()),
        unknown: unknown(),
    });
    assert_round_trip(message::ConfigurationChange {
        values: BTreeMap::from([
            ("theme".to_owned(), "dark".to_owned()),
            ("locale".to_owned(), "en-US".to_owned()),
        ]),
        complete: true,
        unknown: unknown(),
    });
    assert_round_trip(message::Event {
        kind: message::EventKind::from_i32(1),
        attributes: BTreeMap::from([
            ("path".to_owned(), "/tmp/example".to_owned()),
            ("origin".to_owned(), "test".to_owned()),
        ]),
        flags: 3,
        unknown: unknown(),
    });
    assert_round_trip(message::ResourceRequest {
        kind: message::ResourceKind::from_i32(1),
        reference: "icon-example".to_owned(),
        unknown: unknown(),
    });
    assert_round_trip(message::ResourceResponse {
        reference: "icon-example".to_owned(),
        found: true,
        content: vec![0, 1, 2, 255],
        media_type: "image/png".to_owned(),
        error: Some(error.clone()),
        unknown: unknown(),
    });
    assert_round_trip(message::LogRecord {
        level: message::LogLevel::from_i32(1),
        message: "started".to_owned(),
        timestamp_ms: 123_456,
        unknown: unknown(),
    });
    assert_round_trip(message::HealthCheck {
        nonce: 0xfeed_beef,
        unknown: unknown(),
    });
    assert_round_trip(message::HealthReport {
        nonce: 0xfeed_beef,
        healthy: true,
        memory_bytes: 65_536,
        queue_depth: 3,
        in_flight: 1,
        detail: "healthy".to_owned(),
        unknown: unknown(),
    });
    assert_round_trip(error);
    assert_round_trip(message::FlowControl {
        credits: 2,
        paused: true,
        unknown: unknown(),
    });
    assert_round_trip(message::Shutdown {
        immediate: true,
        unknown: unknown(),
    });
}

#[test]
fn minimal_messages_pin_frozen_field_numbers() {
    macro_rules! first_key {
        ($message:expr, $expected:expr) => {{
            let encoded = $message.encode();
            assert_eq!(encoded.first().copied(), Some($expected));
        }};
    }

    first_key!(
        message::Envelope {
            connection_id: 1,
            request_id: 0,
            generation: 0,
            deadline_ms: 0,
            payload: None,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::Handshake {
            protocol_version: 1,
            plugin_id: String::new(),
            plugin_version: String::new(),
            capabilities: Vec::new(),
            session_token: String::new(),
            plugin_name: String::new(),
            sdk_version: String::new(),
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::HandshakeAck {
            protocol_version: 1,
            host_capabilities: Vec::new(),
            host_version: String::new(),
            accepted: false,
            reject_reason: String::new(),
            max_frame_bytes: 0,
            initial_credits: 0,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::Lifecycle {
            kind: message::LifecycleKind::from_i32(1),
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::LifecycleAck {
            kind: message::LifecycleKind::from_i32(1),
            ok: false,
            error: None,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::CatalogRequest {
            max_items: 1,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::CatalogBatch {
            items: vec![item()],
            done: false,
            sequence: 0,
            error: None,
            unknown: unknown(),
        },
        0x0a
    );
    first_key!(
        message::SuggestRequest {
            text: "x".to_owned(),
            normalized_text: String::new(),
            selected_item_id: String::new(),
            max_items: 0,
            max_batches: 0,
            unknown: unknown(),
        },
        0x0a
    );
    first_key!(
        message::Action {
            action_id: "x".to_owned(),
            label: String::new(),
            description: String::new(),
            icon_reference: String::new(),
            execution_policy: String::new(),
            applicable_categories: Vec::new(),
            unknown: unknown(),
        },
        0x0a
    );
    first_key!(
        message::Item {
            stable_id: "x".to_owned(),
            label: String::new(),
            description: String::new(),
            target: String::new(),
            category: String::new(),
            search_terms: Vec::new(),
            icon_reference: String::new(),
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
            unknown: unknown(),
        },
        0x0a
    );
    first_key!(
        message::ResultBatch {
            state: message::BatchState::from_i32(1),
            items: Vec::new(),
            sequence: 0,
            error: None,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::Cancel {
            reason: "x".to_owned(),
            unknown: unknown(),
        },
        0x0a
    );
    first_key!(
        message::ExecuteRequest {
            item_id: "x".to_owned(),
            action_id: String::new(),
            argument: String::new(),
            unknown: unknown(),
        },
        0x0a
    );
    first_key!(
        message::ExecuteResult {
            outcome: message::ExecuteOutcomeCode::from_i32(1),
            error: None,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::ConfigurationChange {
            values: BTreeMap::from([("x".to_owned(), "y".to_owned())]),
            complete: false,
            unknown: unknown(),
        },
        0x0a
    );
    first_key!(
        message::Event {
            kind: message::EventKind::from_i32(1),
            attributes: BTreeMap::new(),
            flags: 0,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::ResourceRequest {
            kind: message::ResourceKind::from_i32(1),
            reference: String::new(),
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::ResourceResponse {
            reference: "x".to_owned(),
            found: false,
            content: Vec::new(),
            media_type: String::new(),
            error: None,
            unknown: unknown(),
        },
        0x0a
    );
    first_key!(
        message::LogRecord {
            level: message::LogLevel::from_i32(1),
            message: String::new(),
            timestamp_ms: 0,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::HealthCheck {
            nonce: 1,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::HealthReport {
            nonce: 1,
            healthy: false,
            memory_bytes: 0,
            queue_depth: 0,
            in_flight: 0,
            detail: String::new(),
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::StructuredError {
            code: message::ErrorCode::from_i32(1),
            message: String::new(),
            detail: String::new(),
            request_id: 0,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::FlowControl {
            credits: 1,
            paused: false,
            unknown: unknown(),
        },
        0x08
    );
    first_key!(
        message::Shutdown {
            immediate: true,
            unknown: unknown(),
        },
        0x08
    );
}

#[test]
fn envelope_oneof_tags_are_frozen() {
    let cases = [
        (message::Payload::Handshake(handshake()), vec![0x52]),
        (message::Payload::HandshakeAck(handshake_ack()), vec![0x5a]),
        (
            message::Payload::Suggest(message::SuggestRequest {
                text: "x".to_owned(),
                normalized_text: String::new(),
                selected_item_id: String::new(),
                max_items: 0,
                max_batches: 0,
                unknown: unknown(),
            }),
            vec![0x62],
        ),
        (
            message::Payload::Results(message::ResultBatch {
                state: message::BatchState::from_i32(1),
                items: Vec::new(),
                sequence: 0,
                error: None,
                unknown: unknown(),
            }),
            vec![0x6a],
        ),
        (
            message::Payload::Cancel(message::Cancel {
                reason: "x".to_owned(),
                unknown: unknown(),
            }),
            vec![0x72],
        ),
        (
            message::Payload::Shutdown(message::Shutdown {
                immediate: true,
                unknown: unknown(),
            }),
            vec![0x7a],
        ),
        (
            message::Payload::CatalogRequest(message::CatalogRequest {
                max_items: 1,
                unknown: unknown(),
            }),
            vec![0x82, 0x01],
        ),
        (
            message::Payload::CatalogBatch(message::CatalogBatch {
                items: Vec::new(),
                done: true,
                sequence: 0,
                error: None,
                unknown: unknown(),
            }),
            vec![0x8a, 0x01],
        ),
        (
            message::Payload::Execute(message::ExecuteRequest {
                item_id: "x".to_owned(),
                action_id: String::new(),
                argument: String::new(),
                unknown: unknown(),
            }),
            vec![0x92, 0x01],
        ),
        (
            message::Payload::ExecuteResult(message::ExecuteResult {
                outcome: message::ExecuteOutcomeCode::from_i32(1),
                error: None,
                unknown: unknown(),
            }),
            vec![0x9a, 0x01],
        ),
        (
            message::Payload::Configuration(message::ConfigurationChange {
                values: BTreeMap::new(),
                complete: true,
                unknown: unknown(),
            }),
            vec![0xa2, 0x01],
        ),
        (
            message::Payload::Event(message::Event {
                kind: message::EventKind::from_i32(1),
                attributes: BTreeMap::new(),
                flags: 0,
                unknown: unknown(),
            }),
            vec![0xaa, 0x01],
        ),
        (
            message::Payload::Log(message::LogRecord {
                level: message::LogLevel::from_i32(1),
                message: String::new(),
                timestamp_ms: 0,
                unknown: unknown(),
            }),
            vec![0xb2, 0x01],
        ),
        (
            message::Payload::HealthCheck(message::HealthCheck {
                nonce: 1,
                unknown: unknown(),
            }),
            vec![0xba, 0x01],
        ),
        (
            message::Payload::HealthReport(message::HealthReport {
                nonce: 1,
                healthy: false,
                memory_bytes: 0,
                queue_depth: 0,
                in_flight: 0,
                detail: String::new(),
                unknown: unknown(),
            }),
            vec![0xc2, 0x01],
        ),
        (message::Payload::Error(structured_error()), vec![0xca, 0x01]),
        (
            message::Payload::Flow(message::FlowControl {
                credits: 1,
                paused: false,
                unknown: unknown(),
            }),
            vec![0xd2, 0x01],
        ),
        (
            message::Payload::ResourceRequest(message::ResourceRequest {
                kind: message::ResourceKind::from_i32(1),
                reference: String::new(),
                unknown: unknown(),
            }),
            vec![0xda, 0x01],
        ),
        (
            message::Payload::ResourceResponse(message::ResourceResponse {
                reference: "x".to_owned(),
                found: false,
                content: Vec::new(),
                media_type: String::new(),
                error: None,
                unknown: unknown(),
            }),
            vec![0xe2, 0x01],
        ),
        (
            message::Payload::Lifecycle(message::Lifecycle {
                kind: message::LifecycleKind::from_i32(1),
                unknown: unknown(),
            }),
            vec![0xea, 0x01],
        ),
        (
            message::Payload::LifecycleAck(message::LifecycleAck {
                kind: message::LifecycleKind::from_i32(1),
                ok: true,
                error: None,
                unknown: unknown(),
            }),
            vec![0xf2, 0x01],
        ),
    ];

    for (payload, key) in cases {
        let encoded = message::Envelope {
            connection_id: 0,
            request_id: 0,
            generation: 0,
            deadline_ms: 0,
            payload: Some(payload),
            unknown: unknown(),
        }
        .encode();
        assert!(encoded.starts_with(&key), "payload key changed: {encoded:?}");
    }
}

#[test]
fn unknown_fields_survive_after_known_fields_and_unknown_payload() {
    let known = item().encode();
    let unknown_bytes = [0xa0, 0x06, 0x07, 0xaa, 0x06, 0x01, b'z'];
    let mut input = unknown_bytes.to_vec();
    input.extend_from_slice(&known);
    let decoded = message::Item::decode(&input).expect("unknown fields are forward-compatible");
    assert_eq!(decoded.unknown.as_bytes(), &unknown_bytes[..]);
    let mut expected = known;
    expected.extend_from_slice(&unknown_bytes);
    assert_eq!(decoded.encode(), expected);

    let unknown_payload = [0x9a, 0x06, 0x01, 0xff];
    let envelope = message::Envelope::decode(&unknown_payload)
        .expect("unknown oneof payloads must be retained, not rejected");
    assert!(envelope.payload.is_none());
    assert_eq!(envelope.unknown.as_bytes(), &unknown_payload[..]);
    assert_eq!(envelope.encode(), unknown_payload.to_vec());
}

fn assert_malformed_without_panic(bytes: &[u8]) {
    let result = catch_unwind(AssertUnwindSafe(|| message::Envelope::decode(bytes)));
    match result {
        Ok(Err(ProtocolError::Malformed(_))) => {}
        Ok(other) => panic!("adversarial bytes returned {other:?}: {bytes:?}"),
        Err(_) => panic!("decoder panicked for adversarial bytes: {bytes:?}"),
    }
}

#[test]
fn message_decoding_is_total_for_adversarial_bytes() {
    let valid = message::Envelope {
        connection_id: 0,
        request_id: 0,
        generation: 0,
        deadline_ms: 0,
        payload: Some(message::Payload::Cancel(message::Cancel {
            reason: "truncated".to_owned(),
            unknown: unknown(),
        })),
        unknown: unknown(),
    }
    .encode();
    for end in 1..valid.len() {
        assert_malformed_without_panic(&valid[..end]);
    }

    for bytes in [
        vec![0x00],
        vec![0x0b],
        vec![0x0f],
        vec![0x08, 0x80],
        vec![0xff; 10],
        vec![0x72, 0x05, 0x0a, 0x02, b'x'],
        vec![0x72, 0x7f],
        vec![0x0d, 0, 0, 0, 0],
    ] {
        assert_malformed_without_panic(&bytes);
    }
}

fn core_item(category: Category, stable_id: &str, target: &str) -> Item {
    Item {
        stable_id: ItemId(stable_id.to_owned()),
        plugin_id: PluginId("dev.example.native".to_owned()),
        category,
        label: "Core item".to_owned(),
        description: "Core description".to_owned(),
        target: target.to_owned(),
        search_terms: vec!["core".to_owned(), "native".to_owned()],
        icon_reference: Some("icon-core".to_owned()),
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 27,
        metadata: BTreeMap::from([("a".to_owned(), "1".to_owned()), ("b".to_owned(), "2".to_owned())]),
        actions: vec![Action {
            action_id: ActionId("open".to_owned()),
            label: "Open".to_owned(),
            description: "Open it".to_owned(),
            // Non-empty on purpose: with an empty vector the equality check in
            // `assert_core_item_eq` compares nothing, and a codec that drops
            // `applicable_categories` entirely still passes (spec 10.4).
            applicable_categories: vec![
                Category::Application,
                Category::PluginDefined("documents".to_owned()),
            ],
            icon_reference: Some("icon-open".to_owned()),
            execution_policy: ExecutionPolicy::Plugin,
        }],
    }
}

fn assert_core_item_eq(actual: &Item, expected: &Item) {
    assert_eq!(actual.stable_id, expected.stable_id);
    assert_eq!(actual.plugin_id, expected.plugin_id);
    assert_eq!(actual.category, expected.category);
    assert_eq!(actual.label, expected.label);
    assert_eq!(actual.description, expected.description);
    assert_eq!(actual.target, expected.target);
    assert_eq!(actual.search_terms, expected.search_terms);
    assert_eq!(actual.icon_reference, expected.icon_reference);
    assert_eq!(actual.argument_policy, expected.argument_policy);
    assert_eq!(actual.hit_policy, expected.hit_policy);
    assert_eq!(actual.score_hint, expected.score_hint);
    assert_eq!(actual.metadata, expected.metadata);
    assert_eq!(actual.actions.len(), expected.actions.len());
    for (actual, expected) in actual.actions.iter().zip(&expected.actions) {
        assert_eq!(actual.action_id, expected.action_id);
        assert_eq!(actual.label, expected.label);
        assert_eq!(actual.description, expected.description);
        assert_eq!(actual.applicable_categories, expected.applicable_categories);
        assert_eq!(actual.icon_reference, expected.icon_reference);
        assert_eq!(actual.execution_policy, expected.execution_policy);
    }
}

#[test]
fn category_conversion_round_trips_and_empty_stable_id_is_derived() {
    let categories = [
        Category::Application,
        Category::File,
        Category::Directory,
        Category::Url,
        Category::Command,
        Category::Expression,
        Category::Keyword,
        Category::Contact,
        Category::ClipboardItem,
        Category::PluginDefined("documents".to_owned()),
    ];
    let plugin = PluginId("dev.example.native".to_owned());

    for (index, category) in categories.into_iter().enumerate() {
        let tag = crikey_native_protocol::convert::category_tag(&category);
        assert_eq!(crikey_native_protocol::convert::category_from_tag(&tag), category);
        let original = core_item(category, &format!("stable-{index}"), &format!("target-{index}"));
        let proto = crikey_native_protocol::convert::to_proto_item(&original);
        let decoded = crikey_native_protocol::convert::from_proto_item(&plugin, &proto);
        assert_core_item_eq(&decoded, &original);
    }

    let original = core_item(Category::Url, "ignored", "https://example.test");
    let mut proto = crikey_native_protocol::convert::to_proto_item(&original);
    proto.stable_id.clear();
    let decoded = crikey_native_protocol::convert::from_proto_item(&plugin, &proto);
    assert_eq!(
        decoded.stable_id,
        ItemId::derived(&plugin, &original.category, &original.target)
    );
    assert_eq!(decoded.plugin_id, plugin);
}

#[test]
fn endpoint_specs_are_total_and_round_trip() {
    let cases = [
        (
            "unix:/run/crikey/x.sock",
            Endpoint::UnixSocket(std::path::PathBuf::from("/run/crikey/x.sock")),
        ),
        ("pipe:crikey-x", Endpoint::NamedPipe("crikey-x".to_owned())),
        ("stdio", Endpoint::Stdio),
    ];

    for (spec, endpoint) in cases {
        let parsed = Endpoint::parse(spec).expect("frozen endpoint spec must parse");
        assert_eq!(parsed, endpoint);
        assert_eq!(parsed.to_spec(), spec);
    }
    for garbage in ["", "tcp:localhost:1", "unix:", "pipe:", "stdio:extra"] {
        assert!(matches!(
            Endpoint::parse(garbage),
            Err(ProtocolError::Malformed(_))
        ));
    }
}
