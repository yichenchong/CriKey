//! Turning icon bytes into pixels (spec 6.4, 11.7).
//!
//! Four formats, chosen by what desktops actually ship rather than by what a
//! general image crate offers:
//!
//! * **PNG** -- the raster half of every Linux icon theme, and the frame
//!   encoding inside every modern `.ico` and `.icns`.
//! * **SVG** -- the other half, and on a current GNOME the larger one: Adwaita
//!   ships 715 SVGs against 51 PNGs, so a PNG-only build finds nothing for most
//!   themed names.
//! * **ICO** -- Windows shortcut and executable icon resources.
//! * **ICNS** -- macOS application bundle icons.
//!
//! The two container formats are parsed here rather than pulled in as crates.
//! Both are a directory of frames whose payload is, in every case this decodes,
//! a PNG or a Windows DIB; the walk is short, and a crate for each would add a
//! second copy of the PNG decoder that is already present. Parsing them here
//! also means the Windows and macOS icon paths are exercised by the suite on
//! *any* host, which is the same reason `Info.plist` parsing lives in
//! `crikey-platform` instead of the macOS backend.
//!
//! Every function here treats its input as hostile. A container's offsets and
//! lengths are checked against the buffer that carries them, a frame's declared
//! dimensions are checked against the spec 11.7 bounds before a buffer is
//! allocated for them, and a frame that fails either is skipped or refused --
//! never trusted far enough to index with.

use std::path::Path;

use super::{check_dimensions, IconError, IconImage, MAX_ICON_EDGE, MAX_ICON_PIXELS};
/// Limits applied to the parsed SVG tree before resvg can allocate its
/// intermediate layers. The encoded payload limit is not sufficient: filters
/// and isolated groups can each allocate an off-screen pixmap.
const MAX_SVG_NODES: usize = 100_000;
const MAX_SVG_DEPTH: usize = 128;
const MAX_SVG_OFFSCREEN_PIXELS: u64 = MAX_ICON_PIXELS * 32;

/// The formats this build decodes.
///
/// Public because it is also the answer to "is this file worth locating at
/// all": [`PathIconSource`](super::PathIconSource) and the XDG theme search
/// both refuse an extension that would only fail later, so a `.svgz` or an
/// `.xpm` in a theme directory is passed over in favour of a sibling this
/// decoder can read instead of being located and then rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconFormat {
    Png,
    Svg,
    Ico,
    Icns,
}

impl IconFormat {
    /// The format a file name claims, matched case insensitively because a
    /// theme directory contains `.PNG` files as often as anybody hand-copies
    /// icons out of a Windows share.
    pub fn from_extension(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("png") {
            Some(Self::Png)
        } else if extension.eq_ignore_ascii_case("svg") {
            Some(Self::Svg)
        } else if extension.eq_ignore_ascii_case("ico") {
            Some(Self::Ico)
        } else if extension.eq_ignore_ascii_case("icns") {
            Some(Self::Icns)
        } else {
            None
        }
    }

    /// The format the bytes themselves claim.
    ///
    /// The content decides, not the extension: theme directories are full of
    /// `.png` files that are really SVGs and vice versa, and a decoder chosen by
    /// file name would reject them for a reason that has nothing to do with the
    /// image.
    fn sniff(bytes: &[u8]) -> Option<Self> {
        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
        const ICNS: &[u8] = b"icns";
        // Reserved word zero, then image type 1 (icon) or 2 (cursor).
        const ICO: &[u8] = &[0, 0, 1, 0];
        const CUR: &[u8] = &[0, 0, 2, 0];

        if bytes.starts_with(PNG) {
            return Some(Self::Png);
        }
        if bytes.starts_with(ICNS) {
            return Some(Self::Icns);
        }
        if bytes.starts_with(ICO) || bytes.starts_with(CUR) {
            return Some(Self::Ico);
        }
        looks_like_svg(bytes).then_some(Self::Svg)
    }
}

/// Whether the leading bytes open an XML document that declares an `<svg>` root.
///
/// SVG has no magic number, so the check is structural: skip a UTF-8 byte-order
/// mark and whitespace, require `<`, and then require the `<svg` element within
/// the prologue -- which is where a real document's comments, processing
/// instructions and doctype live. The window is bounded so that a multi-megabyte
/// file of angle brackets cannot be scanned end to end before being rejected.
fn looks_like_svg(bytes: &[u8]) -> bool {
    /// Long enough for an XML declaration, a doctype and a comment or two.
    const PROLOGUE: usize = 1024;

    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    if bytes.get(start) != Some(&b'<') {
        return false;
    }
    let window = &bytes[start..bytes.len().min(start + PROLOGUE)];
    window.windows(4).any(|candidate| candidate == b"<svg")
}

/// Decodes `bytes` into pixels, preferring a frame or render whose edge is
/// `size`.
///
/// `size` is a preference, not a promise: a raster file has the dimensions it
/// has, a container is asked for its closest frame, and only a vector source can
/// honour the request exactly.
pub fn decode_icon(reference: &str, bytes: &[u8], size: u32) -> Result<IconImage, IconError> {
    match IconFormat::sniff(bytes) {
        Some(IconFormat::Png) => decode_png(reference, bytes),
        Some(IconFormat::Svg) => decode_svg(reference, bytes, size),
        Some(IconFormat::Ico) => decode_ico(reference, bytes, size),
        Some(IconFormat::Icns) => decode_icns(reference, bytes, size),
        None => Err(IconError::UnsupportedFormat {
            reference: reference.to_owned(),
            detail: "no PNG, SVG, ICO or ICNS signature".to_owned(),
        }),
    }
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

/// Decodes a PNG to straight RGBA8.
///
/// The declared dimensions are checked against the spec 11.7 bounds *from the
/// header*, before the output buffer is sized: a 900-byte file declaring
/// 30000x30000 is the cheap way to ask a launcher for 3.6 GiB, and the whole
/// point of the check is that the allocation never happens. The decoder's own
/// byte limit is set as well, so the intermediate buffers it chooses are bounded
/// even for a shape that passes the dimension check.
fn decode_png(reference: &str, bytes: &[u8]) -> Result<IconImage, IconError> {
    let malformed = |detail: String| IconError::Malformed {
        reference: reference.to_owned(),
        detail,
    };

    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Palette entries and 1/2/4-bit greys become 8-bit channels and 16-bit
    // channels are reduced to 8, so every accepted PNG leaves exactly one of
    // the four 8-bit colour types below.
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::EXPAND);
    decoder.set_limits(png::Limits {
        bytes: (MAX_ICON_PIXELS * 4) as usize,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|error| malformed(error.to_string()))?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    check_dimensions(reference, width, height)?;

    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| malformed("the decoded size does not fit in this address space".to_owned()))?;
    let mut buffer = vec![0_u8; buffer_size];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|error| malformed(error.to_string()))?;
    let pixels = &buffer[..frame.buffer_size()];

    let rgba = match reader.output_color_type() {
        (png::ColorType::Rgba, png::BitDepth::Eight) => pixels.to_vec(),
        (png::ColorType::Rgb, png::BitDepth::Eight) => expand(pixels, 3, |source, target| {
            target[..3].copy_from_slice(source);
            target[3] = 0xff;
        }),
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => expand(pixels, 2, |source, target| {
            target[..3].fill(source[0]);
            target[3] = source[1];
        }),
        (png::ColorType::Grayscale, png::BitDepth::Eight) => expand(pixels, 1, |source, target| {
            target[..3].fill(source[0]);
            target[3] = 0xff;
        }),
        (color, depth) => {
            return Err(IconError::UnsupportedFormat {
                reference: reference.to_owned(),
                detail: format!("{color:?} at {depth:?} survived normalization to 8-bit colour"),
            })
        }
    };
    IconImage::new(reference, width, height, rgba)
}

/// Widens `channels`-per-pixel samples into RGBA8.
fn expand(pixels: &[u8], channels: usize, mut widen: impl FnMut(&[u8], &mut [u8; 4])) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixels.len() / channels * 4);
    for source in pixels.chunks_exact(channels) {
        let mut target = [0_u8; 4];
        widen(source, &mut target);
        rgba.extend_from_slice(&target);
    }
    rgba
}

// ---------------------------------------------------------------------------
// SVG
// ---------------------------------------------------------------------------

/// Rasterises an SVG at `size` pixels on its longer edge.
///
/// A vector source is the one case where the requested size can be honoured
/// exactly, so it is: the icon is rendered at the size the row will draw it,
/// which is sharper than scaling a 16x16 raster up and cheaper than scaling a
/// 512x512 one down on every frame. Parsing and rendering are bounded before
/// any pixmap is allocated.
fn decode_svg(reference: &str, bytes: &[u8], size: u32) -> Result<IconImage, IconError> {
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(bytes, &options).map_err(|error| IconError::Malformed {
        reference: reference.to_owned(),
        detail: error.to_string(),
    })?;
    preflight_svg(reference, tree.root())?;

    let intrinsic = tree.size();
    let longest = intrinsic.width().max(intrinsic.height());
    if !longest.is_finite() || longest <= 0.0 {
        return Err(IconError::Malformed {
            reference: reference.to_owned(),
            detail: format!("intrinsic size {}x{}", intrinsic.width(), intrinsic.height()),
        });
    }
    let requested = size.clamp(1, MAX_ICON_EDGE);
    let scale = requested as f32 / longest;
    // Rounding a sub-pixel edge to zero would produce an empty pixmap, and an
    // extreme aspect ratio is what makes that reachable from a real file.
    let width = ((intrinsic.width() * scale).round() as u32).clamp(1, MAX_ICON_EDGE);
    let height = ((intrinsic.height() * scale).round() as u32).clamp(1, MAX_ICON_EDGE);
    check_dimensions(reference, width, height)?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).ok_or_else(|| IconError::Malformed {
        reference: reference.to_owned(),
        detail: format!("cannot allocate a {width}x{height} raster"),
    })?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // `tiny-skia` composites in premultiplied alpha; `IconImage` is straight
    // alpha, so this conversion is what keeps a vector icon from being drawn
    // with a dark fringe.
    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for pixel in pixmap.pixels() {
        let straight = pixel.demultiply();
        rgba.extend_from_slice(&[
            straight.red(),
            straight.green(),
            straight.blue(),
            straight.alpha(),
        ]);
    }
    IconImage::new(reference, width, height, rgba)
}

fn preflight_svg(reference: &str, root: &resvg::usvg::Group) -> Result<(), IconError> {
    fn walk(group: &resvg::usvg::Group, depth: usize, nodes: &mut usize, offscreen: &mut u64) -> bool {
        if depth > MAX_SVG_DEPTH {
            return false;
        }
        for node in group.children() {
            *nodes = nodes.saturating_add(1);
            if *nodes > MAX_SVG_NODES {
                return false;
            }
            if let resvg::usvg::Node::Group(child) = node {
                if child.should_isolate() {
                    let bounds = child.abs_layer_bounding_box();
                    let area = (bounds.width().max(0.0) as f64 * bounds.height().max(0.0) as f64) as u64;
                    *offscreen = offscreen.saturating_add(area);
                    if *offscreen > MAX_SVG_OFFSCREEN_PIXELS {
                        return false;
                    }
                }
                if !walk(child, depth + 1, nodes, offscreen) {
                    return false;
                }
            }
        }
        true
    }

    let mut nodes = 0;
    let mut offscreen = 0;
    if walk(root, 0, &mut nodes, &mut offscreen) {
        Ok(())
    } else {
        Err(IconError::Malformed {
            reference: reference.to_owned(),
            detail: "SVG tree exceeds bounded node, depth, or off-screen area budget".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// ICO
// ---------------------------------------------------------------------------

/// Bytes of the `ICONDIR` header.
const ICO_HEADER: usize = 6;
/// Bytes of one `ICONDIRENTRY`.
const ICO_ENTRY: usize = 16;
/// Bytes of the `BITMAPINFOHEADER` that opens a non-PNG frame.
const DIB_HEADER: usize = 40;

/// Decodes the frame of a Windows `.ico` closest to `size`.
///
/// A `.ico` is a directory of independent images; picking one is the whole job,
/// and the choice is the same rule the theme search uses: the smallest frame at
/// least as large as the request, or the largest frame when every one of them is
/// smaller. Scaling down is what preserves detail, and scaling a 16x16 up into a
/// 48x48 row is the outcome worth avoiding.
///
/// A frame is either a PNG or a Windows DIB, and a frame that is neither -- or
/// whose recorded extent falls outside the file -- is skipped rather than
/// failing the icon: a container with one broken frame and three good ones is
/// still an icon.
fn decode_ico(reference: &str, bytes: &[u8], size: u32) -> Result<IconImage, IconError> {
    let count = u16::from_le_bytes([
        *bytes.get(4).ok_or_else(|| truncated(reference, "ICONDIR"))?,
        *bytes.get(5).ok_or_else(|| truncated(reference, "ICONDIR"))?,
    ]);
    let mut best: Option<(u32, &[u8])> = None;
    for index in 0..usize::from(count) {
        let entry = ICO_HEADER + index * ICO_ENTRY;
        let Some(entry) = bytes.get(entry..entry + ICO_ENTRY) else {
            break;
        };
        // A zero in the byte-sized dimension fields means 256: the format
        // predates icons that large and never widened the field.
        let width = if entry[0] == 0 { 256 } else { u32::from(entry[0]) };
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let length = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let Some(frame) = offset.checked_add(length).and_then(|end| bytes.get(offset..end)) else {
            continue;
        };
        if better(width, best.map(|(edge, _)| edge), size) {
            best = Some((width, frame));
        }
    }

    let Some((_, frame)) = best else {
        return Err(IconError::Malformed {
            reference: reference.to_owned(),
            detail: format!("none of the {count} declared frames lies inside the file"),
        });
    };
    if IconFormat::sniff(frame) == Some(IconFormat::Png) {
        return decode_png(reference, frame);
    }
    decode_dib(reference, frame)
}

/// Decodes one bottom-up Windows DIB frame out of a `.ico`.
///
/// The frame's `BITMAPINFOHEADER` declares twice the real height, because the
/// colour rows are followed by a 1-bit AND mask of the same height. The mask is
/// what makes a 24-bit frame transparent at all, so it is read rather than
/// skipped; a 32-bit frame carries its own alpha and the mask is redundant
/// there, but Windows still honours it, so it is applied to both.
///
/// Both the colour rows and the mask are padded to a 4-byte boundary, and both
/// are stored bottom row first.
fn decode_dib(reference: &str, frame: &[u8]) -> Result<IconImage, IconError> {
    let header = frame
        .get(..DIB_HEADER)
        .ok_or_else(|| truncated(reference, "BITMAPINFOHEADER"))?;
    let width = i32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let doubled = i32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    let bits = u16::from_le_bytes([header[14], header[15]]);
    let compression = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
    if compression != 0 {
        return Err(IconError::UnsupportedFormat {
            reference: reference.to_owned(),
            detail: format!("DIB compression {compression} is not BI_RGB"),
        });
    }
    if width <= 0 || doubled <= 0 {
        return Err(IconError::Malformed {
            reference: reference.to_owned(),
            detail: format!("DIB extent {width}x{doubled}"),
        });
    }
    // An icon DIB stores colour rows then an AND mask, so the recorded height
    // is twice the image's. A frame that recorded the plain height is a
    // top-down bitmap this format does not define.
    let width = width as u32;
    let height = (doubled as u32) / 2;
    check_dimensions(reference, width, height)?;

    let palette_entries = match bits {
        1 | 4 | 8 => 1_usize << bits,
        24 | 32 => 0,
        other => {
            return Err(IconError::UnsupportedFormat {
                reference: reference.to_owned(),
                detail: format!("{other}-bit DIB frames are not decoded"),
            })
        }
    };
    let palette = frame
        .get(DIB_HEADER..DIB_HEADER + palette_entries * 4)
        .ok_or_else(|| truncated(reference, "DIB palette"))?;
    let body = &frame[DIB_HEADER + palette_entries * 4..];

    let colour_stride = padded_stride(width, u32::from(bits));
    let mask_stride = padded_stride(width, 1);
    let colour_bytes = colour_stride * height as usize;
    let colour = body
        .get(..colour_bytes)
        .ok_or_else(|| truncated(reference, "DIB colour rows"))?;
    // The mask is what a 24-bit frame's transparency lives in, but a truncated
    // one must not delete the image: a missing mask means fully opaque.
    let mask = body.get(colour_bytes..colour_bytes + mask_stride * height as usize);

    let mut rgba = vec![0_u8; (width as usize) * (height as usize) * 4];
    for row in 0..height as usize {
        // Bottom-up storage: the first stored row is the last displayed one.
        let source = &colour[(height as usize - 1 - row) * colour_stride..][..colour_stride];
        let mask_row = mask.map(|mask| &mask[(height as usize - 1 - row) * mask_stride..][..mask_stride]);
        for column in 0..width as usize {
            let (red, green, blue, mut alpha) = match bits {
                // DIBs store blue first.
                32 => {
                    let pixel = &source[column * 4..][..4];
                    (pixel[2], pixel[1], pixel[0], pixel[3])
                }
                24 => {
                    let pixel = &source[column * 3..][..3];
                    (pixel[2], pixel[1], pixel[0], 0xff)
                }
                _ => {
                    let index = sub_byte_index(source, column, u32::from(bits));
                    let entry = palette
                        .get(index * 4..index * 4 + 4)
                        .ok_or_else(|| truncated(reference, "DIB palette entry"))?;
                    (entry[2], entry[1], entry[0], 0xff)
                }
            };
            // A set mask bit means "transparent here", so it clears alpha
            // rather than setting it.
            if let Some(mask_row) = mask_row {
                if sub_byte_index(mask_row, column, 1) == 1 {
                    alpha = 0;
                }
            }
            let target = (row * width as usize + column) * 4;
            rgba[target..target + 4].copy_from_slice(&[red, green, blue, alpha]);
        }
    }
    IconImage::new(reference, width, height, rgba)
}

/// The 4-byte-aligned byte length of one DIB row.
fn padded_stride(width: u32, bits: u32) -> usize {
    ((width as usize) * (bits as usize)).div_ceil(32) * 4
}

/// One 1-, 4- or 8-bit sample out of a packed row, most significant bit first.
fn sub_byte_index(row: &[u8], column: usize, bits: u32) -> usize {
    let per_byte = 8 / bits as usize;
    let byte = row.get(column / per_byte).copied().unwrap_or(0);
    let shift = 8 - bits as usize * (column % per_byte + 1);
    usize::from((byte >> shift) & ((1_u16 << bits) - 1) as u8)
}

// ---------------------------------------------------------------------------
// ICNS
// ---------------------------------------------------------------------------

/// Bytes of one ICNS chunk header: a four-character type and a big-endian
/// length that counts the header itself.
const ICNS_CHUNK_HEADER: usize = 8;

/// The chunk types whose payload is a PNG, with the edge each one names.
///
/// Only the PNG-bearing types are listed. The older ones hold run-length encoded
/// 1-, 4- and 8-bit bitmaps whose masks live in separate chunks, and the
/// `ic04`/`ic05` pair holds raw ARGB; a bundle old enough to carry only those is
/// reported as undecodable rather than half-read, and every macOS release that
/// CriKey targets writes `ic07` and up.
const ICNS_PNG_TYPES: [(&[u8; 4], u32); 8] = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"icp6", 64),
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic13", 256),
];

/// Decodes the frame of a macOS `.icns` closest to `size`.
///
/// The 512 and 1024 pixel types (`ic09`, `ic10`, `ic14`) are deliberately not
/// candidates: they exceed [`MAX_ICON_EDGE`] and would be refused after being
/// chosen, which in a bundle that also ships a 128-pixel frame would turn a
/// perfectly usable icon into an error.
fn decode_icns(reference: &str, bytes: &[u8], size: u32) -> Result<IconImage, IconError> {
    let mut best: Option<(u32, &[u8])> = None;
    let mut offset = ICNS_CHUNK_HEADER;
    while let Some(header) = bytes.get(offset..offset + ICNS_CHUNK_HEADER) {
        let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if length < ICNS_CHUNK_HEADER {
            // A chunk that does not even span its own header cannot be walked
            // past, so the walk stops rather than looping on it forever.
            break;
        }
        let Some(payload) = bytes.get(offset + ICNS_CHUNK_HEADER..offset + length) else {
            break;
        };
        let edge = ICNS_PNG_TYPES
            .iter()
            .find(|(name, _)| *name == &header[..4])
            .map(|(_, edge)| *edge);
        if let Some(edge) = edge {
            if IconFormat::sniff(payload) == Some(IconFormat::Png) && better(edge, best.map(|(e, _)| e), size)
            {
                best = Some((edge, payload));
            }
        }
        offset += length;
    }

    let Some((_, payload)) = best else {
        return Err(IconError::UnsupportedFormat {
            reference: reference.to_owned(),
            detail: "no PNG-bearing icon chunk of a decodable size".to_owned(),
        });
    };
    decode_png(reference, payload)
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// Whether `candidate` is a better match for `requested` than `current`.
///
/// The smallest candidate at least as large as the request wins; when every
/// candidate is smaller, the largest of them does. Downscaling keeps detail that
/// upscaling cannot invent, which is why "too big" beats "too small".
fn better(candidate: u32, current: Option<u32>, requested: u32) -> bool {
    let Some(current) = current else {
        return true;
    };
    match (candidate >= requested, current >= requested) {
        (true, true) => candidate < current,
        (true, false) => true,
        (false, true) => false,
        (false, false) => candidate > current,
    }
}

fn truncated(reference: &str, what: &str) -> IconError {
    IconError::Malformed {
        reference: reference.to_owned(),
        detail: format!("truncated before the {what}"),
    }
}
