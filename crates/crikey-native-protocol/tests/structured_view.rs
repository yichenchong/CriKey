use crikey_core::structured_view::{RouteAdmission, RouteId, RouteIdentity, SurfaceIdentity};
use crikey_core::PluginId;
use crikey_native_protocol::{message as m, structured_view, Message, ProtocolError};
use std::num::NonZeroU64;

fn admission() -> RouteAdmission {
    RouteAdmission::worker(
        RouteIdentity {
            owner: PluginId("image-owner".into()),
            route: RouteId("grid".into()),
            incarnation: NonZeroU64::MIN,
        },
        SurfaceIdentity {
            logical_view: NonZeroU64::MIN,
            surface: NonZeroU64::MIN,
            generation: NonZeroU64::MIN,
        },
    )
    .unwrap()
}
fn png() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 512, 512);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&vec![0; 512 * 512 * 4]).unwrap();
    }
    bytes
}
fn decoded_descriptor(count: usize, image: &[u8]) -> m::ViewDescriptor {
    let mut definitions = Vec::new();
    let mut items = Vec::new();
    for index in 0..count {
        let id = format!("image-{index}");
        definitions.push(m::ViewItemDefinition {
            item: Some(m::Item {
                stable_id: id.clone(),
                label: id.clone(),
                category: "command".into(),
                argument_policy: "forbidden".into(),
                hit_policy: "ignored".into(),
                ..Default::default()
            }),
            actionless: true,
            action_sections: vec![m::ViewActionSection {
                id: "actions".into(),
                label: "Actions".into(),
                entries: vec![m::ViewMenuEntry {
                    kind: 1,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        });
        items.push(m::ViewItemPresentation {
            item: Some(m::ItemPresentation {
                item_id: id,
                ..Default::default()
            }),
            image: Some(m::PreviewResponse {
                status: 1,
                encoded_png: image.to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    let envelope = m::Envelope {
        connection_id: 1,
        request_id: 1,
        payload: Some(m::Payload::StructuredViewResponse(m::StructuredViewResponse {
            route_id: "grid".into(),
            logical_view: 1,
            surface: 1,
            status: 1,
            descriptor: Some(Box::new(m::ViewDescriptor {
                title: "Images".into(),
                definitions,
                kind: 2,
                columns: 2,
                collection: Some(m::ViewCollection {
                    items,
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        })),
        ..Default::default()
    };
    let bytes = envelope.encode();
    assert!(bytes.len() < crikey_native_protocol::MAX_FRAME_BYTES);
    let m::Payload::StructuredViewResponse(response) = m::Envelope::decode(&bytes).unwrap().payload.unwrap()
    else {
        panic!("view response")
    };
    *response.descriptor.unwrap()
}
#[test]
fn compressed_collection_rasters_are_aggregate_bounded_before_expansion() {
    let png = png();
    // Sixty-four compressed frames fit the real wire/decoder budget, but
    // materializing them first would allocate 64 MiB of retained raster alone.
    let descriptor = decoded_descriptor(64, &png);
    assert!(matches!(
        structured_view::from_descriptor(descriptor, &admission()),
        Err(ProtocolError::DecodeBudgetExceeded { .. })
    ));
    // Missing image data would fail inside PNG decoding. Aggregate refusal must
    // precede even the first decoder invocation, including its intermediates.
    let descriptor = decoded_descriptor(64, &png[..33]);
    assert!(matches!(
        structured_view::from_descriptor(descriptor, &admission()),
        Err(ProtocolError::DecodeBudgetExceeded { .. })
    ));
}
#[test]
fn bounded_compressed_collection_still_decodes_actual_pixels() {
    let descriptor = structured_view::from_descriptor(decoded_descriptor(1, &png()), &admission()).unwrap();
    let crikey_core::structured_view::StandardView::Grid(grid) = descriptor.view else {
        panic!("grid")
    };
    let image = grid.collection.items[0].image.as_ref().unwrap();
    assert_eq!(
        (image.width, image.height, image.rgba.len()),
        (512, 512, 1024 * 1024)
    );
}

#[test]
fn png_peak_reserves_all_existing_rgba_buffers_independent_of_order() {
    let mut rgb = vec![0; 512 * 512 * 3];
    let mut state = 0xa5a5_1234u32;
    for byte in &mut rgb {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, 512, 512);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(&rgb).unwrap();
    }
    assert!(encoded.len() < 1024 * 1024);
    for png_first in [true, false] {
        let mut descriptor = decoded_descriptor(7, &png());
        let collection = descriptor.collection.as_mut().unwrap();
        collection.items[0].image = Some(m::PreviewResponse {
            status: 1,
            encoded_png: encoded.clone(),
            ..Default::default()
        });
        for entry in &mut collection.items[1..] {
            entry.image = Some(m::PreviewResponse {
                status: 1,
                width: 512,
                height: 512,
                rgba: vec![0; 1024 * 1024],
                ..Default::default()
            });
        }
        if !png_first {
            collection.items.reverse();
        }
        // Exercise actual bounded wire materialization before host conversion.
        let descriptor = m::ViewDescriptor::decode(&descriptor.encode()).unwrap();
        assert!(matches!(
            structured_view::from_descriptor(descriptor, &admission()),
            Err(ProtocolError::DecodeBudgetExceeded { .. })
        ));
    }
}
