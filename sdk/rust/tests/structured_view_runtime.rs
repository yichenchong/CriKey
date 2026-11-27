//! Real SDK standard-view negotiation, publication budgets and flow control.
#[allow(dead_code)]
#[path = "../../../compatibility/native-conformance/src/bin/structured-view-fixture.rs"]
mod fixture;

use crikey_core::structured_view::*;
use crikey_plugin_sdk::{
    protocol::{
        self,
        message::{self, Envelope, Payload},
    },
    serve_on, ServeConfig,
};
use std::{num::NonZeroU64, thread};

fn frame(request_id: u64, payload: Payload) -> Envelope {
    Envelope {
        connection_id: 1,
        request_id,
        payload: Some(payload),
        ..Default::default()
    }
}

#[test]
fn old_host_gets_explicit_navigation_and_descriptor_refusal() {
    let (mut host, transport) = protocol::transport::pair();
    let worker = thread::spawn(move || {
        let mut config = ServeConfig::new("conformance", "1.0.0");
        config.session_token = Some("token".into());
        serve_on(&mut fixture::Worker, transport, config)
    });
    let Some(Payload::Handshake(handshake)) = host.recv().unwrap().payload else {
        panic!("handshake");
    };
    assert!(handshake
        .capabilities
        .iter()
        .any(|cap| cap == STRUCTURED_VIEW_CAPABILITY));
    host.send(&frame(
        0,
        Payload::HandshakeAck(message::HandshakeAck {
            accepted: true,
            protocol_version: protocol::PROTOCOL_VERSION,
            initial_credits: 32,
            ..Default::default()
        }),
    ))
    .unwrap();
    host.send(&frame(1, Payload::CatalogRequest(Default::default())))
        .unwrap();
    loop {
        let Some(Payload::CatalogBatch(batch)) = host.recv().unwrap().payload else {
            panic!("catalog");
        };
        assert!(batch.error.is_none());
        if batch.done {
            break;
        }
    }
    host.send(&frame(
        2,
        Payload::Execute(message::ExecuteRequest {
            item_id: "launch".into(),
            action_id: "open".into(),
            ..Default::default()
        }),
    ))
    .unwrap();
    let Some(Payload::ExecuteResult(result)) = host.recv().unwrap().payload else {
        panic!("execute refusal");
    };
    assert!(
        result.error.is_some(),
        "navigation cannot silently become Completed"
    );
    assert!(result.navigation.is_none());
    let request = ViewRequest {
        route: RouteId("grid".into()),
        mode: ViewRequestMode::Open,
        session: ViewSessionIdentity {
            logical_view: NonZeroU64::new(1).unwrap(),
            surface: NonZeroU64::new(1).unwrap(),
        },
        previous_revision: None,
        state: None,
        query: String::new(),
        scroll: None,
        reason: ViewRefreshReason::Explicit,
    };
    host.send(&frame(
        3,
        Payload::StructuredViewRequest(protocol::structured_view::to_request(&request).unwrap()),
    ))
    .unwrap();
    let Some(Payload::StructuredViewResponse(response)) = host.recv().unwrap().payload else {
        panic!("descriptor refusal");
    };
    assert_eq!(response.status, 2);
    assert!(response.descriptor.is_none());
    host.send(&frame(4, Payload::Shutdown(Default::default())))
        .unwrap();
    worker.join().unwrap().unwrap();
}

fn start_view_worker(
    credits: u32,
) -> (
    Box<dyn protocol::transport::Transport>,
    thread::JoinHandle<Result<(), crikey_plugin_sdk::SdkError>>,
) {
    let (mut host, transport) = protocol::transport::pair();
    host.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let worker = thread::spawn(move || {
        let mut config = ServeConfig::new("conformance", "1.0.0");
        config.session_token = Some("token".into());
        serve_on(&mut fixture::Worker, transport, config)
    });
    assert!(matches!(
        host.recv().unwrap().payload,
        Some(Payload::Handshake(_))
    ));
    host.send(&frame(
        0,
        Payload::HandshakeAck(message::HandshakeAck {
            accepted: true,
            protocol_version: protocol::PROTOCOL_VERSION,
            host_capabilities: vec![STRUCTURED_VIEW_CAPABILITY.into()],
            initial_credits: credits,
            ..Default::default()
        }),
    ))
    .unwrap();
    (host, worker)
}

fn grid_request(surface: u64, query: &str) -> ViewRequest {
    ViewRequest {
        route: RouteId("grid".into()),
        mode: ViewRequestMode::Open,
        session: ViewSessionIdentity {
            logical_view: NonZeroU64::new(1).unwrap(),
            surface: NonZeroU64::new(surface).unwrap(),
        },
        previous_revision: None,
        state: None,
        query: query.into(),
        scroll: None,
        reason: ViewRefreshReason::Explicit,
    }
}

fn view_frame(request_id: u64, request: &ViewRequest) -> Envelope {
    frame(
        request_id,
        Payload::StructuredViewRequest(protocol::structured_view::to_request(request).unwrap()),
    )
}

#[test]
fn core_valid_shared_images_get_bounded_failure_without_exiting_worker() {
    let owner = crikey_core::PluginId("conformance".into());
    let one = NonZeroU64::new(1).unwrap();
    let admission = RouteAdmission::worker(
        RouteIdentity {
            owner: owner.clone(),
            route: RouteId("grid".into()),
            incarnation: one,
        },
        SurfaceIdentity {
            logical_view: one,
            surface: one,
            generation: one,
        },
    )
    .unwrap();
    let descriptor = fixture::oversized_shared_images(&owner);
    descriptor.validate(&admission).unwrap();
    let StandardView::Grid(grid) = &descriptor.view else {
        panic!("grid fixture");
    };
    assert_eq!(grid.collection.items.len(), 9);
    let image = grid.collection.items[0].image.as_ref().unwrap();
    assert_eq!(image.rgba.len(), 1024 * 1024);
    assert!(grid
        .collection
        .items
        .iter()
        .all(|item| { std::sync::Arc::ptr_eq(image, item.image.as_ref().unwrap()) }));

    let (mut host, worker) = start_view_worker(2);
    host.send(&view_frame(1, &grid_request(1, "oversized-shared-images")))
        .unwrap();
    let envelope = host.recv().unwrap();
    assert_eq!(envelope.request_id, 1);
    message::validate_outbound(&envelope).unwrap();
    let Some(Payload::StructuredViewResponse(response)) = envelope.payload else {
        panic!("bounded view refusal");
    };
    assert_eq!(response.status, 4);
    assert!(response.descriptor.is_none());

    host.send(&view_frame(2, &grid_request(2, ""))).unwrap();
    let envelope = host.recv().unwrap();
    assert_eq!(envelope.request_id, 2);
    let Some(Payload::StructuredViewResponse(response)) = envelope.payload else {
        panic!("healthy subsequent Open");
    };
    assert_eq!(response.status, 1);
    assert!(response.descriptor.is_some());
    host.send(&frame(3, Payload::Shutdown(Default::default())))
        .unwrap();
    worker.join().unwrap().unwrap();
}

#[test]
fn zero_credit_cancellation_and_deadline_refuse_without_admitting_descriptors() {
    for cancelled in [true, false] {
        let (mut host, worker) = start_view_worker(0);
        let mut request = view_frame(1, &grid_request(1, ""));
        if !cancelled {
            request.deadline_ms = 200;
        }
        host.send(&request).unwrap();
        host.set_read_timeout(Some(std::time::Duration::from_millis(20)))
            .unwrap();
        assert!(
            matches!(host.recv(), Err(protocol::ProtocolError::Timeout)),
            "a descriptor must not be sent without credit"
        );
        host.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        if cancelled {
            host.send(&frame(1, Payload::Cancel(Default::default()))).unwrap();
        }
        let envelope = host.recv().unwrap();
        assert_eq!(envelope.request_id, 1);
        message::validate_outbound(&envelope).unwrap();
        let Some(Payload::StructuredViewResponse(response)) = envelope.payload else {
            panic!("terminal view refusal without credit");
        };
        assert_eq!(response.status, if cancelled { 3 } else { 5 });
        assert!(response.descriptor.is_none());

        host.send(&view_frame(2, &grid_request(2, ""))).unwrap();
        host.set_read_timeout(Some(std::time::Duration::from_millis(20)))
            .unwrap();
        assert!(
            matches!(host.recv(), Err(protocol::ProtocolError::Timeout)),
            "a terminal refusal must not grant success credit"
        );
        host.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        host.send(&frame(
            2,
            Payload::Flow(message::FlowControl {
                credits: 1,
                ..Default::default()
            }),
        ))
        .unwrap();
        let envelope = host.recv().unwrap();
        assert_eq!(envelope.request_id, 2);
        let Some(Payload::StructuredViewResponse(response)) = envelope.payload else {
            panic!("credited descriptor");
        };
        assert_eq!(response.status, 1);
        assert!(response.descriptor.is_some());
        host.send(&frame(3, Payload::Shutdown(Default::default())))
            .unwrap();
        worker.join().unwrap().unwrap();
    }
}
