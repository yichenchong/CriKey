use crikey_core::{
    PreviewContent, PreviewImage, PreviewOutcome, PreviewText, PreviewValue, PreviewValueKind,
};
use crikey_native_protocol::{
    message::{Envelope, Payload, PreviewResponse},
    preview::{from_response, to_response},
    Message,
};

#[test]
fn preview_round_trips_identity_content_and_unknown_fields() {
    let content = PreviewOutcome::Content(PreviewContent {
        text: Some(PreviewText::Markdown("# Selected item".into())),
        image: Some(PreviewImage {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        }),
        values: vec![PreviewValue {
            label: "Site".into(),
            value: "https://example.org".into(),
            kind: PreviewValueKind::Link,
        }],
    });
    let response = to_response(content.clone()).unwrap();
    let envelope = Envelope {
        connection_id: 17,
        request_id: 29,
        generation: 41,
        payload: Some(Payload::PreviewResponse(response)),
        ..Default::default()
    };
    let mut bytes = envelope.encode();
    bytes.extend_from_slice(&[0xc0, 0x0c, 0x13]); // Unknown varint field 200 = 19.
    let decoded = Envelope::decode(&bytes).unwrap();
    assert_eq!(
        (decoded.connection_id, decoded.request_id, decoded.generation),
        (17, 29, 41)
    );
    assert_eq!(decoded.encode(), bytes);
    let Some(Payload::PreviewResponse(response)) = decoded.payload else {
        panic!("missing preview response")
    };
    assert_eq!(from_response(response).unwrap(), content);
}

#[test]
fn duplicated_response_fragments_merge_without_losing_content() {
    let mut bytes = Envelope {
        payload: Some(Payload::PreviewResponse(PreviewResponse {
            status: 1,
            text: "selected".into(),
            ..Default::default()
        })),
        ..Default::default()
    }
    .encode();
    bytes.extend(
        Envelope {
            payload: Some(Payload::PreviewResponse(PreviewResponse {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode(),
    );
    let Some(Payload::PreviewResponse(response)) = Envelope::decode(&bytes).unwrap().payload else {
        panic!("missing preview")
    };
    assert_eq!(response.text, "selected");
    assert!(from_response(response).is_ok());
}

#[test]
fn bounded_decode_and_explicit_noncontent_statuses() {
    for (status, expected) in [
        (2, PreviewOutcome::Empty),
        (3, PreviewOutcome::Unsupported),
        (4, PreviewOutcome::Cancelled),
    ] {
        assert_eq!(
            from_response(PreviewResponse {
                status,
                ..Default::default()
            })
            .unwrap(),
            expected
        );
    }
    assert!(from_response(PreviewResponse {
        status: 2,
        text: "hidden content".into(),
        ..Default::default()
    })
    .is_err());
    assert!(from_response(PreviewResponse {
        status: 999,
        ..Default::default()
    })
    .is_err());
    let bytes = PreviewResponse {
        text: "x".repeat(crikey_core::preview::MAX_PREVIEW_TEXT_BYTES + 1),
        ..Default::default()
    }
    .encode();
    assert!(PreviewResponse::decode(&bytes).is_err());
    let bytes = PreviewResponse {
        values: vec![Default::default(); 129],
        ..Default::default()
    }
    .encode();
    assert!(PreviewResponse::decode(&bytes).is_err());
    // Parsing is deliberately a separate off-thread preparation step, once,
    // not repeated during transport validation and renderer preparation.
    let PreviewOutcome::Content(content) = from_response(PreviewResponse {
        status: 1,
        text: "<script>bad</script>".into(),
        markdown: true,
        ..Default::default()
    })
    .unwrap() else {
        panic!("missing source")
    };
    let Some(PreviewText::Markdown(source)) = content.text else {
        panic!("missing markdown")
    };
    assert!(crikey_core::preview::parse_preview_markdown(&source).is_err());
}

#[test]
fn encoded_png_is_bounded_and_decoded_to_pixels() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[12, 34, 56, 255]).unwrap();
    }
    let PreviewOutcome::Content(content) = from_response(PreviewResponse {
        status: 1,
        encoded_png: bytes.clone(),
        ..Default::default()
    })
    .unwrap() else {
        panic!("no image")
    };
    assert_eq!(content.image.unwrap().rgba, [12, 34, 56, 255]);
    assert!(from_response(PreviewResponse {
        status: 1,
        encoded_png: bytes,
        rgba: vec![0; 4],
        ..Default::default()
    })
    .is_err());
    assert!(crikey_native_protocol::preview::decode_png(&vec![0; 1024 * 1024 + 1]).is_err());
    let mut large = Vec::new();
    {
        let _writer = png::Encoder::new(&mut large, 1024, 1024).write_header().unwrap();
    }
    assert!(crikey_native_protocol::preview::decode_png(&large).is_err());
}
