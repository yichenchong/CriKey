//! Turning an exported engine buffer into a protocol `WebFrame`.
//!
//! The engine hands over BGRA, premultiplied, whole-surface, with no damage
//! rectangles and a stride it chooses. The protocol carries RGBA8. The
//! conversion happens here, in the host, rather than in the launcher, for two
//! reasons: the launcher would have to be told the engine's pixel order, which
//! is an engine detail that has no business crossing the process boundary; and
//! the swap costs 0.149 ms per 696x410 frame on the reference machine, which
//! is time the host has and the launcher's UI thread does not.

use crikey_native_protocol::message::{WebFrame, MAX_WEB_FRAME_BYTES, MAX_WEB_FRAME_EDGE};

/// Why an exported buffer could not become a frame.
///
/// Every variant describes the engine handing over something the protocol
/// cannot carry. None of them is recoverable by retrying, so the caller's only
/// sensible response is to drop the frame and say so.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("exported buffer has a zero dimension ({width}x{height})")]
    Empty { width: u32, height: u32 },
    #[error("exported buffer is {width}x{height}, past the {MAX_WEB_FRAME_EDGE} pixel edge limit")]
    TooLarge { width: u32, height: u32 },
    #[error("exported buffer stride {stride} is short of {width} pixels of BGRA")]
    ShortStride { stride: u32, width: u32 },
    #[error("exported buffer holds {available} bytes, {required} needed for {height} rows of {stride}")]
    Truncated {
        available: usize,
        required: usize,
        height: u32,
        stride: u32,
    },
    #[error("converted frame is {bytes} bytes, past the {MAX_WEB_FRAME_BYTES} byte frame limit")]
    TooManyBytes { bytes: usize },
}

/// A reusable conversion buffer and the surface's frame counter.
///
/// One of these per surface, kept for the surface's lifetime: the destination
/// vector is resized once and then written in place, so a steadily rendering
/// page performs no allocation per frame.
#[derive(Debug, Default)]
pub struct FrameConverter {
    rgba: Vec<u8>,
    generation: u64,
}

impl FrameConverter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The generation most recently handed out, zero before the first frame.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Converts one exported buffer and produces the frame to send.
    ///
    /// `bgra` is the engine's mapping and is only borrowed; the returned frame
    /// owns its pixels, because the mapping is revoked the moment the export
    /// callback returns.
    pub fn convert(
        &mut self,
        surface_id: u64,
        bgra: &[u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<WebFrame, FrameError> {
        self.swap_into(bgra, width, height, stride)?;
        self.generation += 1;
        Ok(WebFrame {
            surface_id,
            generation: self.generation,
            pixel_width: width,
            pixel_height: height,
            rgba8: std::mem::take(&mut self.rgba),
            ..Default::default()
        })
    }

    /// Returns a spent frame's buffer so the next conversion can reuse it.
    ///
    /// The protocol takes the pixels by value, so without this the vector is
    /// freed and reallocated on every frame. Callers hand the buffer back
    /// after encoding; a caller that forgets loses only the reuse.
    pub fn recycle(&mut self, mut rgba: Vec<u8>) {
        if rgba.capacity() > self.rgba.capacity() {
            rgba.clear();
            self.rgba = rgba;
        }
    }

    fn swap_into(&mut self, bgra: &[u8], width: u32, height: u32, stride: u32) -> Result<(), FrameError> {
        if width == 0 || height == 0 {
            return Err(FrameError::Empty { width, height });
        }
        if width > MAX_WEB_FRAME_EDGE || height > MAX_WEB_FRAME_EDGE {
            return Err(FrameError::TooLarge { width, height });
        }
        let row_bytes = width as usize * 4;
        if (stride as usize) < row_bytes {
            return Err(FrameError::ShortStride { stride, width });
        }
        // The byte limit is a property of the dimensions alone, so it is
        // answered before the buffer is consulted at all. A 4096x4096 frame is
        // refused for being 64 MiB, not for arriving short -- and an engine
        // that hands over an enormous surface should be told which of the two
        // it did wrong.
        let bytes = row_bytes * height as usize;
        if bytes > MAX_WEB_FRAME_BYTES {
            return Err(FrameError::TooManyBytes { bytes });
        }
        // The last row is only required to hold its pixels, not a whole
        // stride: a mapping sized exactly to the image is legal and common.
        let required = (height as usize - 1) * stride as usize + row_bytes;
        if bgra.len() < required {
            return Err(FrameError::Truncated {
                available: bgra.len(),
                required,
                height,
                stride,
            });
        }

        self.rgba.clear();
        self.rgba.resize(bytes, 0);
        for (row, destination) in self.rgba.chunks_exact_mut(row_bytes).enumerate() {
            let start = row * stride as usize;
            let source = &bgra[start..start + row_bytes];
            for (out, pixel) in destination.chunks_exact_mut(4).zip(source.chunks_exact(4)) {
                // Alpha is untouched: both sides are premultiplied, so this is
                // a channel permutation and not a colour-space conversion.
                out[0] = pixel[2];
                out[1] = pixel[1];
                out[2] = pixel[0];
                out[3] = pixel[3];
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_rows(width: usize, height: usize, stride: usize) -> Vec<u8> {
        // Row-dependent padding, so a converter that reads straight through
        // the buffer instead of honouring the stride produces wrong pixels
        // rather than merely wrong lengths.
        let mut bytes = vec![0xEE; (height - 1) * stride + width * 4];
        // Arithmetic in `usize` and cast afterwards: at the real page width a
        // `40 + x as u8` overflows once x reaches 216, which made this helper
        // usable only for fixtures narrower than any surface we actually
        // draw.
        for y in 0..height {
            for x in 0..width {
                let at = y * stride + x * 4;
                bytes[at] = (10 + y) as u8;
                bytes[at + 1] = (20 + x) as u8;
                bytes[at + 2] = (30 + y) as u8;
                bytes[at + 3] = (40 + x) as u8;
            }
        }
        bytes
    }

    #[test]
    fn swaps_red_and_blue_and_keeps_alpha() {
        let mut converter = FrameConverter::new();
        let source = bgra_rows(2, 2, 8);
        let frame = converter.convert(7, &source, 2, 2, 8).expect("converts");
        assert_eq!(frame.surface_id, 7);
        assert_eq!(frame.pixel_width, 2);
        assert_eq!(frame.pixel_height, 2);
        assert_eq!(
            frame.rgba8,
            vec![
                30, 20, 10, 40, // (0,0): B=10 G=20 R=30 A=40
                30, 21, 10, 41, // (1,0)
                31, 20, 11, 40, // (0,1)
                31, 21, 11, 41, // (1,1)
            ]
        );
        frame.validate().expect("a converted frame is always valid");
    }

    #[test]
    fn honours_padded_stride() {
        // 3 pixels of image in a 32-byte row: 20 bytes of padding that must
        // not appear anywhere in the output.
        let mut converter = FrameConverter::new();
        let source = bgra_rows(3, 4, 32);
        let frame = converter.convert(1, &source, 3, 4, 32).expect("converts");
        assert_eq!(frame.rgba8.len(), 3 * 4 * 4);
        assert!(!frame.rgba8.contains(&0xEE), "padding leaked into the frame");
        for y in 0..4usize {
            for x in 0..3usize {
                let at = (y * 3 + x) * 4;
                assert_eq!(
                    &frame.rgba8[at..at + 4],
                    &[30 + y as u8, 20 + x as u8, 10 + y as u8, 40 + x as u8],
                    "pixel {x},{y}"
                );
            }
        }
    }

    #[test]
    fn odd_width_with_exactly_tight_stride() {
        let mut converter = FrameConverter::new();
        let source = bgra_rows(7, 3, 28);
        let frame = converter.convert(1, &source, 7, 3, 28).expect("converts");
        assert_eq!(frame.rgba8.len(), 7 * 3 * 4);
        assert_eq!(&frame.rgba8[..4], &[30, 20, 10, 40]);
    }

    #[test]
    fn single_row_buffer_need_not_be_stride_sized() {
        let mut converter = FrameConverter::new();
        // One row of 2 pixels: 8 bytes, even though the stride claims 64.
        let source = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let frame = converter.convert(1, &source, 2, 1, 64).expect("converts");
        assert_eq!(frame.rgba8, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn generation_increments_per_frame_and_never_repeats() {
        let mut converter = FrameConverter::new();
        let source = bgra_rows(2, 2, 8);
        assert_eq!(converter.generation(), 0);
        for expected in 1..=5 {
            let frame = converter.convert(1, &source, 2, 2, 8).expect("converts");
            assert_eq!(frame.generation, expected);
            converter.recycle(frame.rgba8);
        }
        assert_eq!(converter.generation(), 5);
    }

    #[test]
    fn a_rejected_buffer_does_not_burn_a_generation() {
        let mut converter = FrameConverter::new();
        let source = bgra_rows(2, 2, 8);
        converter.convert(1, &source, 2, 2, 8).expect("converts");
        assert!(converter.convert(1, &source, 0, 2, 8).is_err());
        let frame = converter.convert(1, &source, 2, 2, 8).expect("converts");
        assert_eq!(frame.generation, 2, "a dropped frame must not skip a generation");
    }

    #[test]
    fn rejects_zero_dimensions() {
        let mut converter = FrameConverter::new();
        assert_eq!(
            converter.convert(1, &[0; 16], 0, 2, 8),
            Err(FrameError::Empty { width: 0, height: 2 })
        );
        assert_eq!(
            converter.convert(1, &[0; 16], 2, 0, 8),
            Err(FrameError::Empty { width: 2, height: 0 })
        );
    }

    #[test]
    fn rejects_stride_shorter_than_a_row() {
        let mut converter = FrameConverter::new();
        assert_eq!(
            converter.convert(1, &[0; 64], 4, 2, 12),
            Err(FrameError::ShortStride { stride: 12, width: 4 })
        );
    }

    #[test]
    fn rejects_a_truncated_mapping() {
        let mut converter = FrameConverter::new();
        // Four rows of 8 bytes needs 32; offer 31.
        assert_eq!(
            converter.convert(1, &[0; 31], 2, 4, 8),
            Err(FrameError::Truncated {
                available: 31,
                required: 32,
                height: 4,
                stride: 8
            })
        );
    }

    #[test]
    fn rejects_an_edge_past_the_protocol_limit() {
        let mut converter = FrameConverter::new();
        let edge = MAX_WEB_FRAME_EDGE + 1;
        assert_eq!(
            converter.convert(1, &[], edge, 1, edge * 4),
            Err(FrameError::TooLarge {
                width: edge,
                height: 1
            })
        );
    }

    #[test]
    fn rejects_a_frame_past_the_byte_limit_before_allocating_it() {
        let mut converter = FrameConverter::new();
        // Inside the edge limit on both axes, past the byte limit together.
        let edge = MAX_WEB_FRAME_EDGE;
        assert_eq!(
            converter.convert(1, &[], edge, edge, edge * 4),
            Err(FrameError::TooManyBytes {
                bytes: edge as usize * edge as usize * 4
            })
        );
    }

    #[test]
    fn the_real_page_viewport_fits_the_frame_budget() {
        // The measured page viewport. If this ever stops fitting, the cap and
        // the page size have to be reconciled deliberately.
        let mut converter = FrameConverter::new();
        let source = bgra_rows(696, 410, 696 * 4);
        let frame = converter
            .convert(1, &source, 696, 410, 696 * 4)
            .expect("converts");
        assert_eq!(frame.rgba8.len(), 1_141_440);
        frame.validate().expect("fits the protocol bounds");
    }

    #[test]
    fn recycling_reuses_the_allocation() {
        let mut converter = FrameConverter::new();
        let source = bgra_rows(64, 64, 256);
        let frame = converter.convert(1, &source, 64, 64, 256).expect("converts");
        let address = frame.rgba8.as_ptr();
        converter.recycle(frame.rgba8);
        let next = converter.convert(1, &source, 64, 64, 256).expect("converts");
        assert_eq!(next.rgba8.as_ptr(), address, "the buffer was not reused");
    }
}
