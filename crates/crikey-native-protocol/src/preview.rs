//! Preview validation/conversion; call on provider workers, never during paint.
use crate::message;
use crikey_core::preview::{MAX_PREVIEW_IMAGE_BYTES, MAX_PREVIEW_IMAGE_EDGE, MAX_PREVIEW_VALUE_BYTES};
use crikey_core::{
    PreviewContent, PreviewError, PreviewImage, PreviewOutcome, PreviewText, PreviewValue, PreviewValueKind,
};

pub fn from_response(response: message::PreviewResponse) -> Result<PreviewOutcome, String> {
    if response.values.len() > crikey_core::preview::MAX_PREVIEW_VALUES {
        return Err("too many preview values".into());
    }
    if response.detail.len() > MAX_PREVIEW_VALUE_BYTES {
        return Err("preview diagnostic exceeds bounds".into());
    }
    if response.status != 1 {
        if !response.text.is_empty()
            || response.markdown
            || response.width != 0
            || response.height != 0
            || !response.rgba.is_empty()
            || !response.encoded_png.is_empty()
            || !response.values.is_empty()
        {
            return Err("non-content preview carries content".into());
        }
        return match response.status {
            2 => Ok(PreviewOutcome::Empty),
            3 => Ok(PreviewOutcome::Unsupported),
            4 => Ok(PreviewOutcome::Cancelled),
            5 => Err(response.detail),
            _ => Err("unknown preview status".into()),
        };
    }
    let image = if !response.encoded_png.is_empty() {
        if !response.rgba.is_empty() || response.width != 0 || response.height != 0 {
            return Err("encoded and decoded image representations are mutually exclusive".into());
        }
        Some(decode_png(&response.encoded_png).map_err(|e| e.to_string())?)
    } else if response.width != 0 || response.height != 0 || !response.rgba.is_empty() {
        Some(PreviewImage {
            width: response.width,
            height: response.height,
            rgba: response.rgba,
        })
    } else {
        None
    };
    let mut values = Vec::with_capacity(response.values.len());
    for value in response.values {
        values.push(PreviewValue {
            label: value.label,
            value: value.value,
            kind: match value.kind {
                1 => PreviewValueKind::Text,
                2 => PreviewValueKind::Link,
                3 => PreviewValueKind::Tag,
                _ => return Err("unknown preview metadata kind".into()),
            },
        });
    }
    let content = PreviewContent {
        text: if response.text.is_empty() {
            None
        } else if response.markdown {
            Some(PreviewText::Markdown(response.text))
        } else {
            Some(PreviewText::Plain(response.text))
        },
        image,
        values,
    };
    content.validate().map_err(|e| e.to_string())?;
    Ok(PreviewOutcome::Content(content))
}

pub fn to_response(outcome: PreviewOutcome) -> Result<message::PreviewResponse, PreviewError> {
    let mut response = message::PreviewResponse::default();
    match outcome {
        PreviewOutcome::Empty => response.status = 2,
        PreviewOutcome::Unsupported => response.status = 3,
        PreviewOutcome::Cancelled => response.status = 4,
        PreviewOutcome::Content(content) => {
            content.validate()?;
            response.status = 1;
            if let Some(text) = content.text {
                match text {
                    PreviewText::Plain(s) => response.text = s,
                    PreviewText::Markdown(s) => {
                        response.text = s;
                        response.markdown = true;
                    }
                }
            }
            if let Some(image) = content.image {
                response.width = image.width;
                response.height = image.height;
                response.rgba = image.rgba;
            }
            response.values = content
                .values
                .into_iter()
                .map(|value| message::PreviewValue {
                    label: value.label,
                    value: value.value,
                    kind: match value.kind {
                        PreviewValueKind::Text => 1,
                        PreviewValueKind::Link => 2,
                        PreviewValueKind::Tag => 3,
                    },
                    ..Default::default()
                })
                .collect();
        }
    }
    Ok(response)
}

pub fn decode_png(bytes: &[u8]) -> Result<PreviewImage, PreviewError> {
    if bytes.len() > MAX_PREVIEW_IMAGE_BYTES {
        return Err(PreviewError("encoded PNG exceeds bounds"));
    }
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::EXPAND);
    decoder.set_limits(png::Limits {
        bytes: MAX_PREVIEW_IMAGE_BYTES,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|_| PreviewError("invalid PNG header"))?;
    let info = reader.info();
    let (width, height) = (info.width, info.height);
    let size = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .ok_or(PreviewError("PNG dimensions overflow"))? as usize;
    if width == 0
        || height == 0
        || width > MAX_PREVIEW_IMAGE_EDGE
        || height > MAX_PREVIEW_IMAGE_EDGE
        || size > MAX_PREVIEW_IMAGE_BYTES
        || info.animation_control.is_some()
    {
        return Err(PreviewError("PNG dimensions or animation exceed bounds"));
    }
    let buffer_size = reader
        .output_buffer_size()
        .ok_or(PreviewError("PNG output size overflow"))?;
    if buffer_size > MAX_PREVIEW_IMAGE_BYTES {
        return Err(PreviewError("PNG output exceeds bounds"));
    }
    let mut buffer = vec![0; buffer_size];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|_| PreviewError("invalid PNG data"))?;
    buffer.truncate(frame.buffer_size());
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight {
        return Err(PreviewError("unsupported PNG bit depth"));
    }
    let rgba = if color == png::ColorType::Rgba {
        buffer
    } else {
        let channels = match color {
            png::ColorType::Rgb => 3,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Grayscale => 1,
            _ => return Err(PreviewError("unsupported PNG color")),
        };
        let mut rgba = Vec::with_capacity(size);
        for pixel in buffer.chunks_exact(channels) {
            match channels {
                3 => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
                2 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]),
                _ => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
            }
        }
        rgba
    };
    let image = PreviewImage { width, height, rgba };
    image.validate()?;
    Ok(image)
}
