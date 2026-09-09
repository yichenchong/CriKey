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
        // Non-default so the all-fields round-trip covers tags 11 and 12.
        argument_policy: "required".to_owned(),
        hit_policy: "ignored".to_owned(),
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
    assert_eq!(message::PageShapeCode::from_i32(i32::MAX).as_i32(), 0);
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
        argument_policy: String::new(),
        hit_policy: String::new(),
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
        page_id: String::new(),
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
        page_id: String::new(),
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
            argument_policy: String::new(),
            hit_policy: String::new(),
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
            page_id: String::new(),
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
    first_key!(
        message::PageImage {
            pixel_width: 1,
            pixel_height: 0,
            rgba: Vec::new(),
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
                page_id: String::new(),
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
        (
            message::Payload::PageRequest(message::PageRequest {
                page_id: "p".to_owned(),
                generation: 1,
                width: 0,
                height: 0,
                events: Vec::new(),
                focused: false,
                colour_surface: 0,
                colour_text: 0,
                colour_accent: 0,
                colour_muted: 0,
                unknown: unknown(),
            }),
            vec![0xfa, 0x01],
        ),
        (
            message::Payload::PageFrame(message::PageFrame {
                generation: 1,
                title: String::new(),
                nodes: Vec::new(),
                focus_node: 0,
                redraw_after_ms: 0,
                close: false,
                web: None,
                unknown: unknown(),
            }),
            vec![0x82, 0x02],
        ),
        // Web surface traffic. Each carries `surface_id`, so none of these is
        // an empty payload that would encode to a bare key.
        (
            message::Payload::OpenSurface(message::OpenSurface {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0x8a, 0x02],
        ),
        (
            message::Payload::RawInput(message::RawInput {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0x92, 0x02],
        ),
        (
            message::Payload::ImeEvent(message::ImeEvent {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0x9a, 0x02],
        ),
        (
            message::Payload::Navigate(message::Navigate {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0xa2, 0x02],
        ),
        (
            message::Payload::Resize(message::Resize {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0xaa, 0x02],
        ),
        (
            message::Payload::CloseSurface(message::CloseSurface {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0xb2, 0x02],
        ),
        (
            message::Payload::WebFrame(message::WebFrame {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0xba, 0x02],
        ),
        (
            message::Payload::CaretArea(message::CaretArea {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0xc2, 0x02],
        ),
        (
            message::Payload::LoadState(message::LoadState {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0xca, 0x02],
        ),
        (
            message::Payload::Gone(message::Gone {
                surface_id: 1,
                ..Default::default()
            }),
            vec![0xd2, 0x02],
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
        let decoded = message::Envelope::decode(&encoded).expect("each payload variant must decode");
        assert_eq!(decoded.encode(), encoded, "payload round-trip changed bytes");
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
        // Field number 2^29 is outside protobuf's 29-bit field-number range.
        vec![0x80, 0x80, 0x80, 0x80, 0x10],
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
        // Shadowing names: a plugin category merely CALLED "application" is not
        // the built-in one, and core proves it (`ItemId::derived` gives them
        // different identities). An encoding that renders both as
        // "application" silently rewrites the plugin's category and its item
        // identity, so every built-in name is round-tripped in its shadowed
        // form too.
        Category::PluginDefined("application".to_owned()),
        Category::PluginDefined("file".to_owned()),
        Category::PluginDefined("clipboard-item".to_owned()),
        // A name that itself looks like the wire prefix must survive intact.
        Category::PluginDefined("plugin-defined:nested".to_owned()),
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

    // The shadowed pair must stay distinct end to end, including the derived
    // identity core computes from the category.
    for name in ["application", "file", "directory", "url", "command"] {
        let builtin = crikey_native_protocol::convert::category_from_tag(name);
        let shadowed = Category::PluginDefined(name.to_owned());
        assert_ne!(builtin, shadowed, "`{name}` decoded into the plugin-defined form");
        let builtin_tag = crikey_native_protocol::convert::category_tag(&builtin);
        let shadowed_tag = crikey_native_protocol::convert::category_tag(&shadowed);
        assert_ne!(
            builtin_tag, shadowed_tag,
            "built-in `{name}` and a plugin category of the same name share one wire tag"
        );
        assert_eq!(
            crikey_native_protocol::convert::category_from_tag(&shadowed_tag),
            shadowed
        );
        assert_ne!(
            ItemId::derived(&plugin, &builtin, "/same/target"),
            ItemId::derived(&plugin, &shadowed, "/same/target"),
            "a shadowed category must not collapse onto the built-in's item identity"
        );
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
    for garbage in [
        "",
        "tcp:localhost:1",
        "unix:",
        "pipe:",
        "stdio:extra",
        "unix:/tmp/has\0nul",
        "pipe:has\0nul",
    ] {
        assert!(matches!(
            Endpoint::parse(garbage),
            Err(ProtocolError::Malformed(_))
        ));
    }
}

#[test]
fn singular_fields_and_map_entry_keys_use_last_value() {
    let envelope = vec![0x08, 0x01, 0x08, 0x02];
    let decoded = message::Envelope::decode(&envelope).expect("proto3 permits duplicate scalars");
    assert_eq!(decoded.connection_id, 2);
    assert_eq!(decoded.encode(), vec![0x08, 0x02]);

    let action = vec![0x12, 0x01, b'a', 0x12, 0x01, b'b'];
    let decoded = message::Action::decode(&action).expect("proto3 permits duplicate strings");
    assert_eq!(decoded.label, "b");
    assert_eq!(decoded.encode(), vec![0x12, 0x01, b'b']);

    let item = [0x4a, 0x09, 0x0a, 0x01, b'k', 0x12, 0x01, b'v', 0x0a, 0x01, b'x'];
    let decoded = message::Item::decode(&item).expect("map entries use the last key/value");
    assert_eq!(decoded.metadata.get("x"), Some(&"v".to_owned()));
    assert_eq!(
        decoded.encode(),
        vec![0x4a, 0x06, 0x0a, 0x01, b'x', 0x12, 0x01, b'v']
    );
}

#[test]
fn unknown_map_entry_fields_are_retained_as_raw_map_fields() {
    let bytes = [0x4a, 0x08, 0x0a, 0x01, b'k', 0x12, 0x01, b'v', 0x18, 0x01];
    let decoded = message::Item::decode(&bytes).expect("map entry with an unknown field decodes");
    assert!(decoded.metadata.is_empty());
    assert_eq!(decoded.encode(), bytes);
}
#[test]
fn unsigned_and_signed_32_bit_fields_reject_truncating_values() {
    let protocol_version = [0x08, 0x80, 0x80, 0x80, 0x80, 0x10];
    assert!(matches!(
        message::Handshake::decode(&protocol_version),
        Err(ProtocolError::Malformed(message)) if message.contains("uint32")
    ));

    let score_hint = [0x40, 0x80, 0x80, 0x80, 0x80, 0x10];
    assert!(matches!(
        message::Item::decode(&score_hint),
        Err(ProtocolError::Malformed(message)) if message.contains("int32")
    ));

    let enum_value = [0x08, 0x80, 0x80, 0x80, 0x80, 0x10];
    assert!(matches!(
        message::Lifecycle::decode(&enum_value),
        Err(ProtocolError::Malformed(message)) if message.contains("int32")
    ));
}

#[test]
fn invalid_utf8_in_known_string_is_malformed_without_panic() {
    let bytes = [0x12, 0x01, 0xff];
    let result = catch_unwind(AssertUnwindSafe(|| message::Action::decode(&bytes)));
    assert!(matches!(
        result,
        Ok(Err(ProtocolError::Malformed(message))) if message.contains("UTF-8")
    ));
}

#[test]
fn maximum_magnitude_integer_fields_round_trip() {
    assert_round_trip(message::Handshake {
        protocol_version: u32::MAX,
        plugin_id: "plugin".to_owned(),
        plugin_version: String::new(),
        capabilities: Vec::new(),
        session_token: String::new(),
        plugin_name: String::new(),
        sdk_version: String::new(),
        unknown: unknown(),
    });
    assert_round_trip(message::HandshakeAck {
        protocol_version: u32::MAX,
        host_capabilities: Vec::new(),
        host_version: String::new(),
        accepted: false,
        reject_reason: String::new(),
        max_frame_bytes: u64::MAX,
        initial_credits: u32::MAX,
        unknown: unknown(),
    });
    assert_round_trip(message::Item {
        stable_id: String::new(),
        label: String::new(),
        description: String::new(),
        target: String::new(),
        category: String::new(),
        search_terms: Vec::new(),
        icon_reference: String::new(),
        score_hint: i32::MIN,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
        argument_policy: String::new(),
        hit_policy: String::new(),
        unknown: unknown(),
    });
    assert_round_trip(message::Item {
        score_hint: i32::MAX,
        ..message::Item {
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
            argument_policy: String::new(),
            hit_policy: String::new(),
            unknown: unknown(),
        }
    });
    assert_round_trip(message::HealthReport {
        nonce: u64::MAX,
        healthy: true,
        memory_bytes: u64::MAX,
        queue_depth: u32::MAX,
        in_flight: u32::MAX,
        detail: "max".to_owned(),
        unknown: unknown(),
    });
}

#[test]
fn page_geometry_survives_the_float_codec_exactly() {
    // Coordinates are IEEE-754 on the wire, not scaled integers, so a value
    // a plugin computed must come back bit-identical rather than quantised.
    let node = message::PageNode {
        shape: message::PageShapeCode::Rect,
        x: 12.5,
        y: -0.125,
        width: 640.0,
        height: 0.1,
        fill: 0x1234_56ff,
        stroke: 0x00ff_00ff,
        stroke_width: 1.5,
        rounding: 6.0,
        text: "hello".to_owned(),
        text_size: 13.0,
        role: message::PageRoleCode::Button,
        label: "Say hello".to_owned(),
        node_id: 9,
        focus_order: 2,
        checked: true,
        image: None,
        unknown: unknown(),
    };
    let decoded = message::PageNode::decode(&node.encode()).expect("a page node round-trips");
    assert_eq!(decoded, node);
    assert_eq!(decoded.x.to_bits(), 12.5_f32.to_bits());
    assert_eq!(decoded.y.to_bits(), (-0.125_f32).to_bits());
}

#[test]
fn a_not_a_number_coordinate_crosses_the_codec_and_is_refused_above_it() {
    // The codec stays faithful to the bytes; the semantic layer is what
    // refuses them. Proving both halves here keeps the split honest: if the
    // codec ever started sanitising, the second assertion would be testing
    // nothing.
    let node = message::PageNode {
        x: f32::NAN,
        ..message::PageNode::decode(&[]).expect("an empty node is a valid default")
    };
    let decoded = message::PageNode::decode(&node.encode()).expect("NaN is legal protobuf");
    assert!(
        decoded.x.is_nan(),
        "the codec must not silently sanitise a coordinate"
    );

    let frame = message::PageFrame {
        nodes: vec![decoded],
        ..message::PageFrame::decode(&[]).expect("an empty frame is a valid default")
    };
    let core = crikey_native_protocol::convert::from_proto_page_frame(&frame);
    assert_eq!(
        core.validate(),
        Err(crikey_core::PageError::NonFiniteGeometry { index: 0 })
    );
}

#[test]
fn a_page_frame_round_trips_through_both_conversions() {
    let frame = crikey_core::PageFrame {
        generation: 7,
        title: "Preview".to_owned(),
        nodes: vec![crikey_core::PageNode {
            shape: crikey_core::NodeShape::Text,
            x: 8.0,
            y: 4.0,
            text: "Ready".to_owned(),
            fill: crikey_core::PageColor::rgba(235, 238, 242, 255),
            role: crikey_core::NodeRole::Heading,
            label: "Status".to_owned(),
            node_id: 3,
            ..crikey_core::PageNode::default()
        }],
        focus_node: 3,
        redraw_after_ms: 250,
        close: false,
        web: None,
    };
    let wire = crikey_native_protocol::convert::to_proto_page_frame(&frame);
    let bytes = wire.encode();
    let decoded = message::PageFrame::decode(&bytes).expect("a page frame round-trips");
    assert_eq!(
        crikey_native_protocol::convert::from_proto_page_frame(&decoded),
        frame
    );
}

#[test]
fn an_unknown_page_node_field_is_retained_for_forward_compatibility() {
    // A newer plugin drawing with a node property this host has never heard
    // of must not lose it on the way through, which is what ADR-0004's
    // additive-evolution promise means in practice.
    let known = message::PageNode {
        node_id: 4,
        ..message::PageNode::decode(&[]).expect("an empty node is a valid default")
    }
    .encode();
    let mut input = known.clone();
    // Field 200, varint, value 1: beyond anything this schema defines.
    input.extend_from_slice(&[0xc0, 0x0c, 0x01]);
    let decoded = message::PageNode::decode(&input).expect("unknown fields are forward compatible");
    assert_eq!(decoded.node_id, 4);
    assert_eq!(decoded.unknown.as_bytes(), &[0xc0, 0x0c, 0x01]);
    assert_eq!(
        decoded.encode(),
        input,
        "the unknown field must survive re-encoding"
    );
}

#[test]
fn a_page_input_event_round_trips_with_its_modifiers() {
    let input = message::PageInput {
        kind: message::PageInputCode::KeyPressed,
        x: 0.0,
        y: 0.0,
        key: "ArrowDown".to_owned(),
        text: String::new(),
        node_id: 2,
        ctrl: true,
        shift: false,
        alt: true,
        unknown: unknown(),
    };
    let decoded = message::PageInput::decode(&input.encode()).expect("an input event round-trips");
    assert_eq!(decoded, input);
}

#[test]
fn a_raster_node_round_trips_with_its_bytes_intact() {
    // Pixels are the one payload the host does not interpret, so the only
    // useful guarantee is that they arrive byte-identical: a shifted or
    // truncated row would draw plausible-looking garbage instead of failing.
    let rgba: Vec<u8> = (0..2 * 3 * 4).map(|byte| byte as u8 ^ 0xa5).collect();
    let frame = crikey_core::PageFrame {
        generation: 4,
        title: "Swatch".to_owned(),
        nodes: vec![crikey_core::PageNode {
            shape: crikey_core::NodeShape::Image,
            width: 64.0,
            height: 96.0,
            role: crikey_core::NodeRole::Label,
            label: "Colour swatch".to_owned(),
            image: Some(crikey_core::PageImage {
                pixel_width: 2,
                pixel_height: 3,
                rgba: rgba.clone(),
            }),
            ..crikey_core::PageNode::default()
        }],
        focus_node: 0,
        redraw_after_ms: 0,
        close: false,
        web: None,
    };
    frame.validate().expect("a bounded raster is a valid frame");

    let wire = crikey_native_protocol::convert::to_proto_page_frame(&frame);
    // Field 17, length-delimited: the tag the raster is frozen at.
    assert!(
        wire.nodes[0]
            .encode()
            .windows(2)
            .any(|window| window == [0x8a, 0x01]),
        "the raster must be carried on PageNode field 17"
    );
    let decoded = message::PageFrame::decode(&wire.encode()).expect("a raster frame round-trips");
    let restored = crikey_native_protocol::convert::from_proto_page_frame(&decoded);
    assert_eq!(restored, frame);
    assert_eq!(
        restored.nodes[0]
            .image
            .as_ref()
            .expect("the raster survives the wire")
            .rgba,
        rgba
    );
}

/// The other half of the raster-dropping rule. A shape this schema *knows*
/// keeps its raster, so a plugin that attaches one to a rectangle is told so
/// instead of watching it vanish. Only the unknown bucket gives pixels up.
#[test]
fn a_raster_on_a_known_shape_that_cannot_draw_it_is_still_diagnosed() {
    // Field 1 varint 1: RECT, a shape this host knows. Field 17: a 1x1 raster.
    let encoded = [
        0x08, 0x01, 0x8a, 0x01, 0x0a, 0x08, 0x01, 0x10, 0x01, 0x1a, 0x04, 0x01, 0x02, 0x03, 0x04,
    ];
    let node = message::PageNode::decode(&encoded).expect("a rectangle with a raster decodes");
    let frame = crikey_native_protocol::convert::from_proto_page_frame(&message::PageFrame {
        nodes: vec![node],
        ..message::PageFrame::decode(&[]).expect("an empty frame is a valid default")
    });
    assert!(
        frame.nodes[0].image.is_some(),
        "a known shape carries its raster through, so the mismatch is visible"
    );
    assert_eq!(
        frame.validate(),
        Err(crikey_core::PageError::ImageShapeMismatch { index: 0 }),
        "the plugin is told what it got wrong rather than left guessing"
    );
}

/// The case that would have turned forward compatibility into a refused page:
/// a shape this host does not know that *also* carries a raster, which a
/// future shape reusing the payload would produce. Keeping the pixels would
/// leave a raster on a shape that cannot draw one, and `validate` refuses
/// exactly that - so one unknown shape would cost the whole frame.
#[test]
fn an_unknown_shape_carrying_a_raster_still_leaves_a_drawable_frame() {
    // Field 1 varint 99: an unknown shape. Field 17: a 1x1 raster, which this
    // host must not attach to a shape it decoded as `None`.
    let encoded = [
        0x08, 0x63, 0x8a, 0x01, 0x0a, 0x08, 0x01, 0x10, 0x01, 0x1a, 0x04, 0x01, 0x02, 0x03, 0x04,
    ];
    let node = message::PageNode::decode(&encoded).expect("an unknown shape code decodes");
    assert!(
        node.image.is_some(),
        "the wire form still carries what the plugin sent"
    );

    let frame = crikey_native_protocol::convert::from_proto_page_frame(&message::PageFrame {
        nodes: vec![node],
        ..message::PageFrame::decode(&[]).expect("an empty frame is a valid default")
    });
    assert_eq!(frame.nodes[0].shape, crikey_core::NodeShape::None);
    assert_eq!(
        frame.nodes[0].image, None,
        "a raster this host cannot draw is dropped rather than carried into a refusal"
    );
    assert_eq!(
        frame.validate(),
        Ok(()),
        "an unknown shape costs one blank node, never the page"
    );
}

/// Forward compatibility for the shape set, in the case that actually decides
/// whether the policy is safe: an *interactive* node whose shape this host
/// does not know.
///
/// The node degrades to [`NodeShape::None`], which is not a hole but a
/// documented construct — "a focusable, labelled hit target over other
/// drawing". So the control keeps its role, its name and its place in the Tab
/// ring, and only its picture is missing. The alternatives are worse in a way
/// this test exists to prevent anyone quietly adopting: dropping the node
/// leaves a gap in the plugin's focus order and makes the function
/// unreachable, and refusing the frame escalates one unknown shape into
/// killing the page under spec 32.7.
#[test]
fn an_unknown_shape_keeps_an_interactive_node_reachable_rather_than_dropping_it() {
    // Field 1 varint 99: a shape no version of this schema has. Field 12
    // varint 1: BUTTON. Field 13: the label. Field 14 varint 7: the node id.
    let encoded = [
        0x08, 0x63, 0x60, 0x01, 0x6a, 0x05, b'P', b'r', b'i', b'n', b't', 0x70, 0x07,
    ];
    let node = message::PageNode::decode(&encoded).expect("an unknown shape code decodes");
    assert_eq!(node.shape, message::PageShapeCode::ShapeUnspecified);

    let frame = crikey_native_protocol::convert::from_proto_page_frame(&message::PageFrame {
        nodes: vec![node],
        ..message::PageFrame::decode(&[]).expect("an empty frame is a valid default")
    });
    let drawn = &frame.nodes[0];
    assert_eq!(
        drawn.shape,
        crikey_core::NodeShape::None,
        "the unknown shape paints nothing"
    );
    assert_eq!(
        drawn.role,
        crikey_core::NodeRole::Button,
        "the role survives, so the node is still a control and not decoration"
    );
    assert_eq!(
        drawn.accessible_name(),
        Some("Print"),
        "an old host still announces the control it cannot draw"
    );
    assert_eq!(
        frame.focus_ring(),
        vec![7],
        "the control stays reachable by Tab rather than leaving a hole in the ring"
    );
}

// ---------------------------------------------------------------------------
// Web surface (spec 32.x)
// ---------------------------------------------------------------------------

/// Every inner tag of every web message, pinned one field at a time.
///
/// The oneof test above pins the envelope keys; this pins what is inside them,
/// which is where a renumbering would actually hide. Each case sets exactly
/// one field to a non-default value, so the encoding is that field's key
/// followed by its value and nothing else: if a tag moves, the first byte
/// changes and the case that owns it fails by name.
#[test]
fn web_message_field_numbers_are_frozen() {
    macro_rules! only_field {
        ($label:literal, $message:expr, $expected:expr) => {{
            let encoded = $message.encode();
            assert_eq!(
                encoded.first().copied(),
                Some($expected),
                "{} moved: {:?}",
                $label,
                encoded
            );
        }};
    }

    only_field!(
        "OpenSurface.surface_id",
        message::OpenSurface {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "OpenSurface.pixel_width",
        message::OpenSurface {
            pixel_width: 1,
            ..Default::default()
        },
        0x10
    );
    only_field!(
        "OpenSurface.pixel_height",
        message::OpenSurface {
            pixel_height: 1,
            ..Default::default()
        },
        0x18
    );
    only_field!(
        "OpenSurface.url",
        message::OpenSurface {
            url: "x".to_owned(),
            ..Default::default()
        },
        0x22
    );
    only_field!(
        "OpenSurface.storage_mode",
        message::OpenSurface {
            storage_mode: message::WebStorageMode::Persistent,
            ..Default::default()
        },
        0x28
    );

    only_field!(
        "RawInput.surface_id",
        message::RawInput {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "RawInput.kind",
        message::RawInput {
            kind: message::WebInputCode::KeyDown,
            ..Default::default()
        },
        0x10
    );
    only_field!(
        "RawInput.timestamp_ms",
        message::RawInput {
            timestamp_ms: 1,
            ..Default::default()
        },
        0x18
    );
    only_field!(
        "RawInput.x",
        message::RawInput {
            x: 1.0,
            ..Default::default()
        },
        0x25
    );
    only_field!(
        "RawInput.y",
        message::RawInput {
            y: 1.0,
            ..Default::default()
        },
        0x2d
    );
    only_field!(
        "RawInput.keysym",
        message::RawInput {
            keysym: 1,
            ..Default::default()
        },
        0x30
    );
    only_field!(
        "RawInput.hardware_keycode",
        message::RawInput {
            hardware_keycode: 1,
            ..Default::default()
        },
        0x38
    );
    only_field!(
        "RawInput.repeat",
        message::RawInput {
            repeat: true,
            ..Default::default()
        },
        0x40
    );
    only_field!(
        "RawInput.modifiers",
        message::RawInput {
            modifiers: message::WEB_MODIFIER_META,
            ..Default::default()
        },
        0x48
    );
    only_field!(
        "RawInput.button",
        message::RawInput {
            button: message::WEB_POINTER_BUTTON_RIGHT,
            ..Default::default()
        },
        0x50
    );
    only_field!(
        "RawInput.press_count",
        message::RawInput {
            press_count: 2,
            ..Default::default()
        },
        0x58
    );
    only_field!(
        "RawInput.delta_x",
        message::RawInput {
            delta_x: 1.0,
            ..Default::default()
        },
        0x65
    );
    only_field!(
        "RawInput.delta_y",
        message::RawInput {
            delta_y: 1.0,
            ..Default::default()
        },
        0x6d
    );
    only_field!(
        "RawInput.precise",
        message::RawInput {
            precise: true,
            ..Default::default()
        },
        0x70
    );
    only_field!(
        "RawInput.stop",
        message::RawInput {
            stop: true,
            ..Default::default()
        },
        0x78
    );

    only_field!(
        "ImeEvent.surface_id",
        message::ImeEvent {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "ImeEvent.kind",
        message::ImeEvent {
            kind: message::WebImeCode::Commit,
            ..Default::default()
        },
        0x10
    );
    only_field!(
        "ImeEvent.text",
        message::ImeEvent {
            text: "x".to_owned(),
            ..Default::default()
        },
        0x1a
    );
    only_field!(
        "ImeEvent.cursor_begin_chars",
        message::ImeEvent {
            cursor_begin_chars: 1,
            ..Default::default()
        },
        0x20
    );
    only_field!(
        "ImeEvent.cursor_end_chars",
        message::ImeEvent {
            cursor_end_chars: 1,
            ..Default::default()
        },
        0x28
    );
    only_field!(
        "ImeEvent.has_cursor",
        message::ImeEvent {
            has_cursor: true,
            ..Default::default()
        },
        0x30
    );

    only_field!(
        "Navigate.surface_id",
        message::Navigate {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "Navigate.url",
        message::Navigate {
            url: "x".to_owned(),
            ..Default::default()
        },
        0x12
    );

    only_field!(
        "Resize.surface_id",
        message::Resize {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "Resize.pixel_width",
        message::Resize {
            pixel_width: 1,
            ..Default::default()
        },
        0x10
    );
    only_field!(
        "Resize.pixel_height",
        message::Resize {
            pixel_height: 1,
            ..Default::default()
        },
        0x18
    );

    only_field!(
        "CloseSurface.surface_id",
        message::CloseSurface {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "CloseSurface.reason",
        message::CloseSurface {
            reason: "x".to_owned(),
            ..Default::default()
        },
        0x12
    );

    only_field!(
        "WebFrame.surface_id",
        message::WebFrame {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "WebFrame.generation",
        message::WebFrame {
            generation: 1,
            ..Default::default()
        },
        0x10
    );
    only_field!(
        "WebFrame.pixel_width",
        message::WebFrame {
            pixel_width: 1,
            ..Default::default()
        },
        0x18
    );
    only_field!(
        "WebFrame.pixel_height",
        message::WebFrame {
            pixel_height: 1,
            ..Default::default()
        },
        0x20
    );
    only_field!(
        "WebFrame.rgba8",
        message::WebFrame {
            rgba8: vec![1, 2, 3, 4],
            ..Default::default()
        },
        0x2a
    );

    only_field!(
        "CaretArea.surface_id",
        message::CaretArea {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "CaretArea.x",
        message::CaretArea {
            x: 1.0,
            ..Default::default()
        },
        0x15
    );
    only_field!(
        "CaretArea.y",
        message::CaretArea {
            y: 1.0,
            ..Default::default()
        },
        0x1d
    );
    only_field!(
        "CaretArea.width",
        message::CaretArea {
            width: 1.0,
            ..Default::default()
        },
        0x25
    );
    only_field!(
        "CaretArea.height",
        message::CaretArea {
            height: 1.0,
            ..Default::default()
        },
        0x2d
    );

    only_field!(
        "LoadState.surface_id",
        message::LoadState {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "LoadState.state",
        message::LoadState {
            state: message::WebLoadCode::Finished,
            ..Default::default()
        },
        0x10
    );
    only_field!(
        "LoadState.url",
        message::LoadState {
            url: "x".to_owned(),
            ..Default::default()
        },
        0x1a
    );
    only_field!(
        "LoadState.failure",
        message::LoadState {
            failure: "x".to_owned(),
            ..Default::default()
        },
        0x22
    );
    only_field!(
        "LoadState.can_go_back",
        message::LoadState {
            can_go_back: true,
            ..Default::default()
        },
        0x28
    );
    only_field!(
        "LoadState.can_go_forward",
        message::LoadState {
            can_go_forward: true,
            ..Default::default()
        },
        0x30
    );
    only_field!(
        "LoadState.title",
        message::LoadState {
            title: "x".to_owned(),
            ..Default::default()
        },
        0x3a
    );

    only_field!(
        "Gone.surface_id",
        message::Gone {
            surface_id: 1,
            ..Default::default()
        },
        0x08
    );
    only_field!(
        "Gone.reason",
        message::Gone {
            reason: message::WebGoneCode::MemoryLimit,
            ..Default::default()
        },
        0x10
    );
    only_field!(
        "Gone.detail",
        message::Gone {
            detail: "x".to_owned(),
            ..Default::default()
        },
        0x1a
    );

    // The plugin-facing half, which rides on PageFrame rather than on its own
    // envelope key.
    only_field!(
        "PageWebSurface.url",
        message::PageWebSurface {
            url: "x".to_owned(),
            ..Default::default()
        },
        0x0a
    );
    only_field!(
        "PageWebSurface.storage",
        message::PageWebSurface {
            storage: message::WebStorageMode::Persistent,
            ..Default::default()
        },
        0x10
    );
    only_field!(
        "PageFrame.web",
        message::PageFrame {
            web: Some(message::PageWebSurface::default()),
            ..Default::default()
        },
        0x3a
    );
}

#[test]
fn every_web_message_round_trips_all_non_default_fields() {
    assert_round_trip(message::OpenSurface {
        surface_id: 9,
        pixel_width: 696,
        pixel_height: 410,
        url: "https://example.invalid/page".to_owned(),
        storage_mode: message::WebStorageMode::Persistent,
        unknown: unknown(),
    });
    assert_round_trip(message::RawInput {
        surface_id: 9,
        kind: message::WebInputCode::Scroll,
        timestamp_ms: 1_234_567,
        x: 12.5,
        y: -3.25,
        keysym: 0xff09,
        hardware_keycode: 23,
        repeat: true,
        modifiers: message::WEB_MODIFIER_SHIFT
            | message::WEB_MODIFIER_CONTROL
            | message::WEB_MODIFIER_ALT
            | message::WEB_MODIFIER_META
            | message::WEB_MODIFIER_CAPS_LOCK
            | message::WEB_MODIFIER_NUM_LOCK,
        button: message::WEB_POINTER_BUTTON_MIDDLE,
        press_count: 2,
        delta_x: -0.5,
        delta_y: 120.0,
        precise: true,
        stop: true,
        unknown: unknown(),
    });
    assert_round_trip(message::ImeEvent {
        surface_id: 9,
        kind: message::WebImeCode::Preedit,
        text: "你好".to_owned(),
        cursor_begin_chars: 2,
        cursor_end_chars: 2,
        has_cursor: true,
        unknown: unknown(),
    });
    assert_round_trip(message::Navigate {
        surface_id: 9,
        url: "https://example.invalid/next".to_owned(),
        unknown: unknown(),
    });
    assert_round_trip(message::Resize {
        surface_id: 9,
        pixel_width: 800,
        pixel_height: 600,
        unknown: unknown(),
    });
    assert_round_trip(message::CloseSurface {
        surface_id: 9,
        reason: "page closed".to_owned(),
        unknown: unknown(),
    });
    assert_round_trip(message::CaretArea {
        surface_id: 9,
        x: 40.0,
        y: 96.5,
        width: 2.0,
        height: 18.0,
        unknown: unknown(),
    });
    assert_round_trip(message::LoadState {
        surface_id: 9,
        state: message::WebLoadCode::Failed,
        url: "https://example.invalid/page".to_owned(),
        failure: "name not resolved".to_owned(),
        can_go_back: true,
        can_go_forward: true,
        title: "Example".to_owned(),
        unknown: unknown(),
    });
    assert_round_trip(message::Gone {
        surface_id: 9,
        reason: message::WebGoneCode::MemoryLimit,
        detail: "dev.example.web exceeded its ceiling".to_owned(),
        unknown: unknown(),
    });
}

/// The IME cursor is in characters, and an absent cursor is not a cursor at
/// zero. Both halves matter: winit reports UTF-8 *byte* offsets, so a bridge
/// that forwards them unconverted puts WebKit's caret inside a codepoint the
/// moment anyone types the very thing an input method is for.
#[test]
fn an_absent_ime_cursor_is_distinct_from_a_cursor_at_the_start() {
    let absent = message::ImeEvent {
        text: "你好".to_owned(),
        ..Default::default()
    };
    let at_zero = message::ImeEvent {
        text: "你好".to_owned(),
        has_cursor: true,
        ..Default::default()
    };
    assert_ne!(absent.encode(), at_zero.encode());

    let decoded = message::ImeEvent::decode(&at_zero.encode()).expect("a stated cursor decodes");
    assert!(decoded.has_cursor);
    assert_eq!((decoded.cursor_begin_chars, decoded.cursor_end_chars), (0, 0));

    let decoded = message::ImeEvent::decode(&absent.encode()).expect("an absent cursor decodes");
    assert!(!decoded.has_cursor);
}

fn web_frame(pixel_width: u32, pixel_height: u32, bytes: usize) -> message::WebFrame {
    message::WebFrame {
        surface_id: 4,
        generation: 11,
        pixel_width,
        pixel_height,
        rgba8: (0..bytes).map(|byte| byte as u8).collect(),
        unknown: unknown(),
    }
}

/// A frame filled exactly to the cap must survive the whole path: it is the
/// one size where an off-by-one in either the cap or the wire budget turns a
/// legal frame into a disconnected surface.
#[test]
fn a_maximum_size_web_frame_round_trips_and_validates() {
    let pixels = message::MAX_WEB_FRAME_BYTES / 4;
    let frame = web_frame(1024, 512, message::MAX_WEB_FRAME_BYTES);
    assert_eq!(1024 * 512, pixels);
    assert_eq!(frame.validate(), Ok(()));

    let envelope = message::Envelope {
        payload: Some(message::Payload::WebFrame(frame.clone())),
        ..Default::default()
    };
    let encoded = envelope.encode();
    assert!(
        encoded.len() < crikey_native_protocol::MAX_FRAME_BYTES,
        "a frame at the web cap must still fit the wire budget with room to spare"
    );
    let decoded = message::Envelope::decode(&encoded).expect("a capped frame must decode");
    let Some(message::Payload::WebFrame(round_tripped)) = decoded.payload else {
        panic!("the payload must survive as a web frame");
    };
    assert_eq!(
        round_tripped.rgba8, frame.rgba8,
        "the pixels must be byte-identical"
    );
    assert_eq!(round_tripped, frame);
}

#[test]
fn an_oversized_web_frame_is_refused_on_its_byte_count() {
    // One row past the cap, with a buffer that honestly matches the geometry:
    // the size refusal must not depend on the frame also being malformed.
    let bytes = 1024 * 513 * 4;
    let frame = web_frame(1024, 513, bytes);
    assert!(bytes > message::MAX_WEB_FRAME_BYTES);
    assert_eq!(frame.validate(), Err(message::WebFrameError::TooLarge { bytes }));
}

/// A strip is exactly the shape a byte cap cannot catch: 1x524288 is a legal
/// two megabytes and an impossible texture.
#[test]
fn a_web_frame_is_refused_on_its_shape_before_its_size() {
    let frame = web_frame(1, 524_288, message::MAX_WEB_FRAME_BYTES);
    assert_eq!(
        frame.validate(),
        Err(message::WebFrameError::EdgeOutOfRange {
            pixel_width: 1,
            pixel_height: 524_288,
        })
    );

    for (pixel_width, pixel_height) in [(0, 16), (16, 0), (message::MAX_WEB_FRAME_EDGE + 1, 1)] {
        let frame = web_frame(pixel_width, pixel_height, 0);
        assert_eq!(
            frame.validate(),
            Err(message::WebFrameError::EdgeOutOfRange {
                pixel_width,
                pixel_height,
            })
        );
    }
}

/// Declared geometry disagreeing with the buffer is the defect that draws
/// rather than fails: shifted rows look like a rendering bug, not like a
/// truncated frame, so it is named here instead of being uploaded.
#[test]
fn a_web_frame_whose_geometry_disagrees_with_its_bytes_is_refused() {
    let short = web_frame(4, 4, 3);
    assert_eq!(
        short.validate(),
        Err(message::WebFrameError::ByteCountMismatch {
            expected: 64,
            actual: 3,
        })
    );

    let long = web_frame(4, 4, 65);
    assert_eq!(
        long.validate(),
        Err(message::WebFrameError::ByteCountMismatch {
            expected: 64,
            actual: 65,
        })
    );

    // The decoder itself stays total: the bytes materialise and the refusal
    // comes from `validate`, so the diagnostic can name the real defect.
    let decoded = message::WebFrame::decode(&short.encode()).expect("a wrong frame still decodes");
    assert_eq!(decoded, short);
}

/// A newer web host may add a field this launcher has never heard of, and the
/// launcher must hand it back unchanged rather than quietly dropping it.
#[test]
fn an_unknown_web_frame_field_survives_a_round_trip() {
    let known = web_frame(2, 2, 16).encode();
    // Field 99, length-delimited: a tag no version of this schema defines.
    let unknown_bytes = [0x9a, 0x06, 0x02, 0xde, 0xad];
    let mut input = unknown_bytes.to_vec();
    input.extend_from_slice(&known);

    let decoded = message::WebFrame::decode(&input).expect("an unknown field is not a refusal");
    assert_eq!(decoded.unknown.as_bytes(), &unknown_bytes[..]);
    assert_eq!(decoded.pixel_width, 2);
    assert_eq!(decoded.validate(), Ok(()));

    let mut expected = known;
    expected.extend_from_slice(&unknown_bytes);
    assert_eq!(decoded.encode(), expected, "the unknown field must be re-emitted");

    // And the same promise one level up: an unknown field inside a web
    // payload survives being carried in an envelope.
    let envelope = message::Envelope {
        payload: Some(message::Payload::WebFrame(decoded)),
        ..Default::default()
    };
    let bytes = envelope.encode();
    assert_eq!(
        message::Envelope::decode(&bytes)
            .expect("the envelope decodes")
            .encode(),
        bytes
    );
}

/// A plugin declares a web surface on its page frame; the host reads it back
/// as the same thing. The storage mode is the part worth pinning: an
/// unspecified mode must resolve to ephemeral, because the alternative leaves
/// a session on the user's disk that nobody asked for.
#[test]
fn a_web_surface_page_frame_round_trips_through_both_conversions() {
    let frame = crikey_core::PageFrame {
        generation: 3,
        title: "Docs".to_owned(),
        nodes: Vec::new(),
        focus_node: 0,
        redraw_after_ms: 0,
        close: false,
        web: Some(crikey_core::PageWebSurface {
            url: "https://example.invalid/docs".to_owned(),
            storage: crikey_core::WebStorage::Persistent,
        }),
    };
    frame.validate().expect("a web surface page is a valid frame");

    let wire = crikey_native_protocol::convert::to_proto_page_frame(&frame);
    assert_eq!(
        wire.web.as_ref().expect("the surface crosses").storage,
        message::WebStorageMode::Persistent,
        "the mode must be stated on the wire, never left to a silence"
    );
    let bytes = wire.encode();
    let decoded = message::PageFrame::decode(&bytes).expect("a web surface frame decodes");
    assert_eq!(decoded.encode(), bytes);
    assert_eq!(
        crikey_native_protocol::convert::from_proto_page_frame(&decoded),
        frame
    );

    // A peer that leaves the mode at its proto3 default gets the safe answer,
    // not an inherited session.
    let silent = message::PageFrame {
        web: Some(message::PageWebSurface::default()),
        ..Default::default()
    };
    let core = crikey_native_protocol::convert::from_proto_page_frame(&silent);
    assert_eq!(
        core.web.expect("the surface survives").storage,
        crikey_core::WebStorage::Ephemeral
    );
}
