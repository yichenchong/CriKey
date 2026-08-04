//! Icon decoding, the spec 11.7 payload limits, and the decoded-icon cache.
//!
//! Every fixture here is a real encoded image or a real container built to the
//! format's own layout, because the point of the suite is the decoder the
//! launcher runs and not a mock of it. The `.ico` and `.icns` cases matter most
//! on hosts that cannot run them: those containers are what the Windows and
//! macOS backends hand over, and this is the only place either path is
//! exercised.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, SystemTime},
};

use crikey_platform::{
    decode_icon, DirectoryConvention, DirectoryEnvironment, IconCache, IconCacheKey, IconError, IconImage,
    IconLoader, IconProvider, IconSource, PathIconSource, SourceFingerprint, StandardDirectories,
    ICON_CACHE_SCHEMA_VERSION, MAX_ICON_EDGE, MAX_ICON_PAYLOAD_BYTES,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A unique scratch directory that deletes itself when the test ends.
///
/// Uniqueness comes from the process id plus a monotonic counter, never from a
/// clock, so parallel test threads and repeated runs cannot collide.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-icons-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, bytes).expect("fixture is writable");
        path
    }

    /// Standard directories whose every root is inside this scratch directory,
    /// so a cache test writes nowhere near the running user's own cache.
    fn directories(&self) -> StandardDirectories {
        let environment = DirectoryEnvironment::new().set("HOME", &self.path);
        StandardDirectories::resolve(DirectoryConvention::Xdg, &environment)
            .expect("scratch directories resolve")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// An RGBA PNG of one colour.
fn solid_png(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
    let rgba: Vec<u8> = pixel
        .iter()
        .copied()
        .cycle()
        .take((width as usize) * (height as usize) * 4)
        .collect();
    encode_png(width, height, png::ColorType::Rgba, &rgba)
}

fn encode_png(width: u32, height: u32, colour: png::ColorType, samples: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(colour);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("the PNG header is writable");
    writer
        .write_image_data(samples)
        .expect("the PNG body is writable");
    drop(writer);
    out
}

/// A PNG whose `IHDR` declares `width`x`height` and whose body is absent.
///
/// The compressed-bomb shape: a header is all a decoder needs to read before it
/// sizes its output buffer, so a refusal that happens after the body would
/// already have allocated for the declared extent.
fn png_header_only(width: u32, height: u32) -> Vec<u8> {
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlacing.

    let mut out = Vec::from(*b"\x89PNG\r\n\x1a\n");
    push_chunk(&mut out, b"IHDR", &ihdr);
    // A body has to be present for the header to be read at all, but nothing
    // decompresses it before the extent is checked -- which is the point.
    push_chunk(&mut out, b"IDAT", &[0x78, 0x01, 0x01, 0x00, 0x00, 0xff, 0xff]);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc_input = Vec::with_capacity(4 + body.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(body);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// CRC-32 as PNG defines it, so a hand-built chunk is one the decoder accepts.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let carry = crc & 1;
            crc >>= 1;
            if carry != 0 {
                crc ^= 0xedb8_8320;
            }
        }
    }
    !crc
}

fn svg(width: u32, height: u32, fill: &str) -> Vec<u8> {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><rect width="{width}" height="{height}" fill="{fill}"/></svg>"#
    )
    .into_bytes()
}

/// A `.ico` whose frames are the supplied `(declared edge, payload)` pairs.
fn ico(frames: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * frames.len();
    let mut body = Vec::new();
    for (edge, payload) in frames {
        out.extend_from_slice(&[*edge, *edge, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += payload.len();
        body.extend_from_slice(payload);
    }
    out.extend_from_slice(&body);
    out
}

/// A 2x2 24-bit DIB icon frame: two colour rows bottom-up, then a 1-bit AND
/// mask, exactly as a `.ico` stores one.
fn dib_2x2() -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&40_u32.to_le_bytes()); // header size
    frame.extend_from_slice(&2_i32.to_le_bytes()); // width
    frame.extend_from_slice(&4_i32.to_le_bytes()); // height, doubled for the mask
    frame.extend_from_slice(&1_u16.to_le_bytes()); // planes
    frame.extend_from_slice(&24_u16.to_le_bytes()); // bits per pixel
    frame.extend_from_slice(&[0; 24]); // BI_RGB and the fields icons leave zero

    // Stored bottom row first, blue then white, padded to eight bytes.
    frame.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
    // Then the top row: red then green.
    frame.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
    // AND mask, also bottom row first: nothing masked, then the top row's
    // second pixel masked out.
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(&[0b0100_0000, 0, 0, 0]);
    frame
}

/// An `.icns` whose chunks are the supplied `(four-character type, payload)`
/// pairs.
fn icns(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (kind, payload) in chunks {
        body.extend_from_slice(*kind);
        body.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
        body.extend_from_slice(payload);
    }
    let mut out = Vec::from(*b"icns");
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn pixel(image: &IconImage, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * image.width() + x) * 4) as usize;
    image.rgba()[offset..offset + 4]
        .try_into()
        .expect("four channels per pixel")
}

/// Loads `reference` through an uncached loader over absolute paths.
fn load(reference: &Path, size: u32) -> Result<Option<IconImage>, IconError> {
    IconLoader::new(PathIconSource).load(&reference.to_string_lossy(), size)
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[test]
fn a_png_decodes_to_its_declared_extent_with_its_alpha_preserved() {
    let bytes = solid_png(3, 2, [10, 20, 30, 40]);

    let image = decode_icon("app", &bytes, 48).expect("a well-formed PNG decodes");

    assert_eq!((image.width(), image.height()), (3, 2));
    assert_eq!(image.rgba().len(), 3 * 2 * 4);
    assert_eq!(pixel(&image, 2, 1), [10, 20, 30, 40]);
}

#[test]
fn a_png_without_an_alpha_channel_decodes_as_fully_opaque() {
    let bytes = encode_png(1, 2, png::ColorType::Rgb, &[1, 2, 3, 4, 5, 6]);

    let image = decode_icon("app", &bytes, 48).expect("an RGB PNG decodes");

    assert_eq!(pixel(&image, 0, 0), [1, 2, 3, 255]);
    assert_eq!(pixel(&image, 0, 1), [4, 5, 6, 255]);
}

#[test]
fn a_greyscale_png_is_widened_into_three_equal_colour_channels() {
    let bytes = encode_png(2, 1, png::ColorType::Grayscale, &[0x00, 0x7f]);

    let image = decode_icon("app", &bytes, 48).expect("a greyscale PNG decodes");

    assert_eq!(pixel(&image, 0, 0), [0x00, 0x00, 0x00, 0xff]);
    assert_eq!(pixel(&image, 1, 0), [0x7f, 0x7f, 0x7f, 0xff]);
}

#[test]
fn an_svg_is_rasterised_at_the_requested_edge_and_keeps_its_aspect_ratio() {
    let bytes = svg(40, 20, "#0000ff");

    let image = decode_icon("app", &bytes, 48).expect("an SVG rasterises");

    // The longer edge becomes the requested size; the shorter one follows.
    assert_eq!((image.width(), image.height()), (48, 24));
    assert_eq!(pixel(&image, 24, 12), [0, 0, 255, 255]);
}

#[test]
fn a_rasterised_svg_carries_straight_rather_than_premultiplied_alpha() {
    // Half-transparent pure white: premultiplied, this pixel's colour channels
    // would be 0x80 rather than 0xff, which is what a launcher drawing it over
    // its own background would show as grey.
    let bytes = br##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="#ffffff" fill-opacity="0.5"/></svg>"##;

    let image = decode_icon("app", bytes, 8).expect("an SVG rasterises");

    let [red, green, blue, alpha] = pixel(&image, 4, 4);
    assert_eq!((red, green, blue), (0xff, 0xff, 0xff));
    assert!(
        (0x7e..=0x81).contains(&alpha),
        "half-transparent white should keep full colour channels and about half alpha, got {alpha:#x}"
    );
}

#[test]
fn an_ico_answers_with_the_smallest_frame_at_least_as_large_as_the_request() {
    let bytes = ico(&[
        (16, solid_png(16, 16, [1, 0, 0, 255])),
        (32, solid_png(32, 32, [2, 0, 0, 255])),
        (64, solid_png(64, 64, [3, 0, 0, 255])),
    ]);

    let image = decode_icon("app.ico", &bytes, 32).expect("the container decodes");
    assert_eq!(image.width(), 32);

    // Nothing is large enough, so the largest frame wins: downscaling keeps
    // detail that upscaling cannot invent.
    let image = decode_icon("app.ico", &bytes, 128).expect("the container decodes");
    assert_eq!(image.width(), 64);
}

#[test]
fn an_ico_frame_stored_as_a_dib_is_read_bottom_up_with_its_and_mask_applied() {
    let bytes = ico(&[(2, dib_2x2())]);

    let image = decode_icon("app.ico", &bytes, 2).expect("a DIB frame decodes");

    assert_eq!((image.width(), image.height()), (2, 2));
    assert_eq!(pixel(&image, 0, 0), [255, 0, 0, 255], "top left is red");
    assert_eq!(
        pixel(&image, 1, 0),
        [0, 255, 0, 0],
        "the masked top-right pixel keeps its colour and loses its alpha"
    );
    assert_eq!(pixel(&image, 0, 1), [0, 0, 255, 255], "bottom left is blue");
    assert_eq!(pixel(&image, 1, 1), [255, 255, 255, 255], "bottom right is white");
}

#[test]
fn an_ico_frame_pointing_outside_the_file_is_skipped_rather_than_indexed() {
    let mut bytes = ico(&[(16, solid_png(16, 16, [1, 0, 0, 255]))]);
    // Claim a second frame whose directory entry points past the end.
    bytes[4..6].copy_from_slice(&2_u16.to_le_bytes());
    let good = solid_png(8, 8, [2, 0, 0, 255]);
    let _ = good;

    // Only the one real frame remains reachable, so it is what comes back.
    let image = decode_icon("app.ico", &bytes, 16).expect("the reachable frame still decodes");
    assert_eq!(image.width(), 16);
}

#[test]
fn an_icns_answers_from_the_png_chunk_closest_to_the_request() {
    let bytes = icns(&[
        (b"TOC ", vec![0; 16]),
        (b"icp5", solid_png(4, 4, [1, 0, 0, 255])),
        (b"ic07", solid_png(8, 8, [2, 0, 0, 255])),
    ]);

    // `icp5` nominally holds 32 pixels and `ic07` 128, so a 48-pixel request
    // takes `ic07` and a 16-pixel one takes `icp5`.
    let image = decode_icon("App.icns", &bytes, 48).expect("the bundle icon decodes");
    assert_eq!(image.width(), 8);

    let image = decode_icon("App.icns", &bytes, 16).expect("the bundle icon decodes");
    assert_eq!(image.width(), 4);
}

#[test]
fn an_icns_carrying_only_legacy_chunks_is_reported_undecodable_rather_than_guessed() {
    let bytes = icns(&[(b"ICN#", vec![0; 256]), (b"il32", vec![0; 128])]);

    let error = decode_icon("App.icns", &bytes, 48).expect_err("a legacy-only bundle icon is refused");

    assert!(
        matches!(error, IconError::UnsupportedFormat { .. }),
        "expected an unsupported-format refusal, got {error:?}"
    );
}

#[test]
fn an_icns_chunk_that_does_not_span_its_own_header_ends_the_walk() {
    // A zero length would leave the offset where it was: a walk that trusted it
    // would spin forever on a four-byte file any package can ship.
    let mut bytes = Vec::from(*b"icns");
    bytes.extend_from_slice(&16_u32.to_be_bytes());
    bytes.extend_from_slice(b"ic07");
    bytes.extend_from_slice(&0_u32.to_be_bytes());

    let error = decode_icon("App.icns", &bytes, 48).expect_err("a truncated chunk yields no icon");

    assert!(matches!(error, IconError::UnsupportedFormat { .. }));
}

#[test]
fn bytes_matching_no_signature_are_reported_as_an_undecodable_format() {
    let error = decode_icon("app", b"GIF89a not really", 48).expect_err("an unknown format is refused");

    assert!(matches!(error, IconError::UnsupportedFormat { .. }));
}

// ---------------------------------------------------------------------------
// The spec 11.7 payload limits
// ---------------------------------------------------------------------------

#[test]
fn an_icon_file_larger_than_the_payload_limit_is_refused_rather_than_read() {
    let scratch = Scratch::new();
    let oversize = vec![0_u8; (MAX_ICON_PAYLOAD_BYTES + 1) as usize];
    let path = scratch.write("huge.png", &oversize);

    let error = load(&path, 48).expect_err("an oversize payload is refused");

    match error {
        IconError::TooLarge {
            limit, found, what, ..
        } => {
            assert_eq!(limit, MAX_ICON_PAYLOAD_BYTES);
            assert_eq!(found, MAX_ICON_PAYLOAD_BYTES + 1);
            assert_eq!(what, "encoded icon payload in bytes");
        }
        other => panic!("expected a payload-size refusal, got {other:?}"),
    }
}

#[test]
fn a_png_declaring_more_pixels_than_the_limit_is_refused_from_its_header() {
    // 900 bytes on disk, 3.6 GiB of RGBA if believed. The refusal has to come
    // out of the header: by the time a body has been decoded the allocation the
    // limit exists to prevent has already happened.
    let bytes = png_header_only(30_000, 30_000);
    assert!(bytes.len() < 128, "the fixture must be small to make its point");

    let error = decode_icon("bomb.png", &bytes, 48).expect_err("a declared-extent bomb is refused");

    match error {
        IconError::TooLarge {
            limit, found, what, ..
        } => {
            assert_eq!(limit, u64::from(MAX_ICON_EDGE));
            assert_eq!(found, 30_000);
            assert_eq!(what, "icon edge in pixels");
        }
        other => panic!("expected an edge-limit refusal, got {other:?}"),
    }
}

#[test]
fn an_svg_declaring_an_enormous_canvas_still_rasterises_within_the_limit() {
    // A vector source cannot overrun the limit by declaring a large canvas,
    // because the render target is sized from the request rather than from the
    // document. This is the one oversize input that is answered rather than
    // refused, and it must stay answered: a 4096-pixel icon is a perfectly
    // ordinary thing for a theme to ship.
    let bytes = svg(100_000, 100_000, "#00ff00");

    let image = decode_icon("huge.svg", &bytes, 48).expect("an oversize canvas renders at the request");

    assert_eq!((image.width(), image.height()), (48, 48));
}

#[test]
fn a_reference_naming_no_file_is_absent_rather_than_an_error() {
    let scratch = Scratch::new();
    let missing = scratch.path.join("nothing.png");

    let located = load(&missing, 48).expect("a missing icon is not a failure");

    assert!(located.is_none());
}

#[test]
fn a_relative_or_undecodable_path_reference_locates_nothing() {
    let scratch = Scratch::new();
    let unsupported = scratch.write("app.xpm", b"/* XPM */");

    assert!(
        PathIconSource.locate("icons/app.png", 48).is_none(),
        "a relative path names a different file on every run and is refused"
    );
    assert!(
        PathIconSource
            .locate(&unsupported.to_string_lossy(), 48)
            .is_none(),
        "an extension nothing here decodes is passed over rather than located and failed"
    );
}

// ---------------------------------------------------------------------------
// The decoded-icon cache (spec 22.1, 22.4)
// ---------------------------------------------------------------------------

fn cache_key(source: &Path) -> IconCacheKey {
    IconCacheKey {
        reference: "firefox".to_owned(),
        size: 48,
        source: source.to_path_buf(),
        fingerprint: SourceFingerprint::probe(source).expect("the fixture is stat-able"),
    }
}

fn decoded() -> IconImage {
    let bytes = solid_png(4, 4, [7, 8, 9, 255]);
    decode_icon("firefox", &bytes, 48).expect("the fixture decodes")
}

#[test]
fn stored_pixels_are_returned_for_the_same_request() {
    let scratch = Scratch::new();
    let source = scratch.write("firefox.png", &solid_png(4, 4, [7, 8, 9, 255]));
    let cache = IconCache::new(&scratch.directories(), "linux");
    let key = cache_key(&source);
    let image = decoded();

    cache.store(&key, &image);

    assert_eq!(cache.load(&key), Some(image));
}

#[test]
fn a_source_whose_modification_time_changed_invalidates_its_entry() {
    let scratch = Scratch::new();
    let source = scratch.write("firefox.png", &solid_png(4, 4, [7, 8, 9, 255]));
    let cache = IconCache::new(&scratch.directories(), "linux");
    let key = cache_key(&source);
    cache.store(&key, &decoded());
    assert!(cache.load(&key).is_some(), "the entry is there to begin with");

    // A theme package that replaced this file in place is the case the cache has
    // to notice, and the modification time is what notices it. The timestamp is
    // set explicitly rather than by rewriting the file, so the test does not
    // depend on the filesystem's timestamp granularity.
    let handle = fs::File::options()
        .write(true)
        .open(&source)
        .expect("the fixture is writable");
    handle
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(60)))
        .expect("the fixture's timestamp is settable");
    drop(handle);

    assert_eq!(cache.load(&cache_key(&source)), None);
}

#[test]
fn a_source_whose_length_changed_invalidates_its_entry() {
    let scratch = Scratch::new();
    let source = scratch.write("firefox.png", &solid_png(4, 4, [7, 8, 9, 255]));
    let cache = IconCache::new(&scratch.directories(), "linux");
    let stored = cache_key(&source);
    cache.store(&stored, &decoded());

    // An in-place edit that preserves the timestamp -- a restore from backup, an
    // `rsync --archive` -- is why length is checked as well as time.
    let mut replaced = stored.clone();
    replaced.fingerprint.len += 1;

    assert_eq!(cache.load(&replaced), None);
}

#[test]
fn an_entry_written_by_another_backend_is_never_read() {
    let scratch = Scratch::new();
    let source = scratch.write("firefox.png", &solid_png(4, 4, [7, 8, 9, 255]));
    let directories = scratch.directories();
    let key = cache_key(&source);
    let linux = IconCache::new(&directories, "linux");
    let windows = IconCache::new(&directories, "windows");

    // The same reference means something else to another backend: a themed name
    // here, a shortcut icon location there. The backend is part of both the entry
    // name and the entry body, so each layer is checked separately.
    linux.store(&key, &decoded());
    let linux_entry = sole_entry(&scratch);
    let linux_bytes = fs::read(&linux_entry).expect("the entry is readable");
    assert_eq!(windows.load(&key), None, "the name a backend reads is its own");
    assert!(
        linux.load(&key).is_some(),
        "and the backend that wrote it still reads it"
    );

    // Now put the Linux entry where the Windows cache looks, which is what a
    // filename hash collision would amount to.
    windows.store(&key, &decoded());
    let windows_entry = entries(&scratch)
        .into_iter()
        .find(|entry| *entry != linux_entry)
        .expect("the second backend wrote its own entry");
    fs::write(&windows_entry, &linux_bytes).expect("the entry is writable");

    assert_eq!(
        windows.load(&key),
        None,
        "the body records the backend too, so a colliding name is still refused"
    );
}

#[test]
fn an_entry_written_by_another_schema_version_is_ignored() {
    let scratch = Scratch::new();
    let source = scratch.write("firefox.png", &solid_png(4, 4, [7, 8, 9, 255]));
    let cache = IconCache::new(&scratch.directories(), "linux");
    let key = cache_key(&source);
    cache.store(&key, &decoded());

    let entry = sole_entry(&scratch);
    let mut bytes = fs::read(&entry).expect("the entry is readable");
    // The version field follows the eight-byte magic.
    bytes[8..12].copy_from_slice(&(ICON_CACHE_SCHEMA_VERSION + 1).to_le_bytes());
    fs::write(&entry, &bytes).expect("the entry is writable");

    assert_eq!(cache.load(&key), None);
}

#[test]
fn a_truncated_entry_is_a_miss_rather_than_a_short_read() {
    let scratch = Scratch::new();
    let source = scratch.write("firefox.png", &solid_png(4, 4, [7, 8, 9, 255]));
    let cache = IconCache::new(&scratch.directories(), "linux");
    let key = cache_key(&source);
    cache.store(&key, &decoded());

    let entry = sole_entry(&scratch);
    let bytes = fs::read(&entry).expect("the entry is readable");
    fs::write(&entry, &bytes[..bytes.len() - 5]).expect("the entry is writable");

    assert_eq!(cache.load(&key), None);
}

#[test]
fn an_entry_longer_than_its_own_dimensions_is_a_miss() {
    let scratch = Scratch::new();
    let source = scratch.write("firefox.png", &solid_png(4, 4, [7, 8, 9, 255]));
    let cache = IconCache::new(&scratch.directories(), "linux");
    let key = cache_key(&source);
    cache.store(&key, &decoded());

    // A torn write over a larger predecessor leaves a valid header in front of
    // pixels that are not all this image's.
    let entry = sole_entry(&scratch);
    let mut bytes = fs::read(&entry).expect("the entry is readable");
    bytes.extend_from_slice(&[0; 32]);
    fs::write(&entry, &bytes).expect("the entry is writable");

    assert_eq!(cache.load(&key), None);
}

// Unix-only: the test makes the source unreadable with `chmod 000`, which has
// no direct Windows equivalent. The cache behaviour it defends is platform
// independent; only this way of provoking it is not.
#[cfg(unix)]
#[test]
fn a_cached_icon_is_served_without_reading_its_source_again() {
    let scratch = Scratch::new();
    let source = scratch.write("firefox.png", &solid_png(4, 4, [7, 8, 9, 255]));
    let directories = scratch.directories();
    let loader = IconLoader::caching(PathIconSource, "linux", &directories);
    let reference = source.to_string_lossy().into_owned();

    let first = loader
        .load(&reference, 48)
        .expect("the first load decodes")
        .expect("the fixture is located");

    // Unreadable but still stat-able: the fingerprint check still passes, so a
    // second load can only succeed by using the cached pixels.
    fs::set_permissions(&source, permissions(0o000)).expect("permissions are settable");
    let second = loader.load(&reference, 48);
    fs::set_permissions(&source, permissions(0o644)).expect("permissions are settable");

    assert_eq!(
        second.expect("a cached icon does not need its source"),
        Some(first)
    );
}

#[cfg(unix)]
fn permissions(mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;

    fs::Permissions::from_mode(mode)
}

/// Every cache entry the scratch cache directory holds.
fn entries(scratch: &Scratch) -> Vec<PathBuf> {
    let icons = scratch.directories().cache_dir().join("icons");
    fs::read_dir(&icons)
        .expect("the cache directory exists once something was stored")
        .map(|entry| entry.expect("the entry is readable").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "icon"))
        .collect()
}

/// The one cache entry the scratch cache directory holds.
fn sole_entry(scratch: &Scratch) -> PathBuf {
    let mut entries = entries(scratch);
    assert_eq!(entries.len(), 1, "exactly one entry was stored");
    entries.pop().expect("the entry is there")
}

// ---------------------------------------------------------------------------
// Bundle icons (spec 18.5)
// ---------------------------------------------------------------------------
//
// The macOS backend crate is gated on its target and cannot be exercised
// anywhere else, so the part of its icon path that is not an OS binding lives
// here: reading `CFBundleIconFile` and turning it into a file under
// `Contents/Resources`. That is the whole of macOS icon resolution -- the
// reference discovery records is already an absolute path -- so these cases are
// the only place it is checked at all.

/// A bundle directory carrying `Info.plist` and, optionally, a resource.
fn bundle(scratch: &Scratch, icon_file: Option<&str>, resource: Option<(&str, &[u8])>) -> PathBuf {
    let bundle = scratch.path.join("Tool.app");
    let resources = bundle.join("Contents").join("Resources");
    fs::create_dir_all(&resources).expect("bundle is creatable");
    let icon_key = icon_file
        .map(|icon_file| format!("<key>CFBundleIconFile</key><string>{icon_file}</string>"))
        .unwrap_or_default();
    fs::write(
        bundle.join("Contents").join("Info.plist"),
        format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
             <key>CFBundleName</key><string>Tool</string>{icon_key}</dict></plist>"
        ),
    )
    .expect("Info.plist is writable");
    if let Some((name, bytes)) = resource {
        fs::write(resources.join(name), bytes).expect("resource is writable");
    }
    bundle
}

fn icon_file_of(bundle: &Path) -> Option<String> {
    let xml = fs::read_to_string(bundle.join("Contents").join("Info.plist")).expect("plist is readable");
    crikey_platform::parse_info_plist(&xml)
        .expect("the fixture plist parses")
        .icon_file
}

#[test]
fn a_bundle_icon_file_resolves_whether_or_not_it_spells_its_extension() {
    let scratch = Scratch::new();
    // Apple's tools accept both spellings and both ship in real bundles.
    for spelling in ["AppIcon", "AppIcon.icns"] {
        let bundle = bundle(&scratch, Some(spelling), Some(("AppIcon.icns", b"icns")));
        let icon_file = icon_file_of(&bundle).expect("the plist declares an icon file");

        let resolved = crikey_platform::bundle_icon_path(&bundle, &icon_file);

        assert_eq!(
            resolved,
            Some(bundle.join("Contents").join("Resources").join("AppIcon.icns")),
            "{spelling:?} names the bundle's icon"
        );
        fs::remove_dir_all(&bundle).expect("the fixture is removable");
    }
}

#[test]
fn a_bundle_icon_file_the_bundle_does_not_ship_resolves_nothing() {
    let scratch = Scratch::new();
    // `CFBundleIconFile` is author supplied and routinely names a resource a
    // trimmed or relocated bundle no longer has. Reporting a reference nothing
    // can resolve would be worse than reporting none (spec 18.2).
    let bundle = bundle(&scratch, Some("AppIcon"), None);

    assert_eq!(crikey_platform::bundle_icon_path(&bundle, "AppIcon"), None);
}

#[test]
fn a_bundle_icon_file_naming_anything_but_one_plain_component_resolves_nothing() {
    let scratch = Scratch::new();
    let bundle = bundle(&scratch, None, Some(("AppIcon.icns", b"icns")));
    // A bundle is a directory any user can unzip into `~/Applications`. An icon
    // path is a display detail and must not become a way to make the launcher
    // read an arbitrary file.
    let outside = scratch.write("secret", b"not an icon");

    for hostile in [
        "../../../../etc/shadow",
        "..",
        ".",
        "Resources/AppIcon.icns",
        "/etc/shadow",
        &outside.to_string_lossy(),
    ] {
        assert_eq!(
            crikey_platform::bundle_icon_path(&bundle, hostile),
            None,
            "{hostile:?} must resolve to nothing"
        );
    }
}

#[test]
fn a_bundle_icon_resolves_all_the_way_to_decoded_pixels() {
    let scratch = Scratch::new();
    let icns = icns(&[(b"ic07", solid_png(8, 8, [4, 5, 6, 255]))]);
    let bundle = bundle(&scratch, Some("AppIcon.icns"), Some(("AppIcon.icns", &icns)));
    let icon_file = icon_file_of(&bundle).expect("the plist declares an icon file");
    let reference =
        crikey_platform::bundle_icon_path(&bundle, &icon_file).expect("the resource is where it says");

    // The reference discovery records is this absolute path, and the shared path
    // source is what the macOS backend resolves it with.
    let image = IconLoader::new(PathIconSource)
        .load(&reference.to_string_lossy(), 48)
        .expect("the bundle icon decodes")
        .expect("the path resolves");

    assert_eq!((image.width(), image.height()), (8, 8));
    assert_eq!(pixel(&image, 0, 0), [4, 5, 6, 255]);
}

// ---------------------------------------------------------------------------
// Hostile icon files
// ---------------------------------------------------------------------------

/// An [`IconSource`] that hands back one path without checking it.
///
/// Not a contrivance: that is what every source does. [`PathIconSource`] stats
/// the candidate, but the file at that path can be replaced between the stat and
/// the read, so the loader is the last line of defence and this is how that line
/// gets exercised deterministically.
///
/// Unix-only because its sole user is the FIFO test below, and a FIFO is what
/// makes the swap-after-stat race observable without a second thread.
#[cfg(unix)]
#[derive(Debug)]
struct FixedSource(PathBuf);

#[cfg(unix)]
impl IconSource for FixedSource {
    fn locate(&self, _reference: &str, _size: u32) -> Option<PathBuf> {
        Some(self.0.clone())
    }
}

#[cfg(unix)]
#[test]
fn a_fifo_handed_to_the_loader_is_refused_instead_of_waited_on() {
    let scratch = Scratch::new();
    let fifo = scratch.path.join("app.png");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo is available on a Unix host");
    assert!(status.success(), "mkfifo could not create the fixture");

    // On a worker thread with a bounded wait: a loader that opens without
    // `O_NONBLOCK` blocks until a writer appears, which never happens, so a
    // regression has to fail this test by name rather than hang the suite.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let loader = IconLoader::new(FixedSource(fifo));
        let _ = sender.send(loader.load("app", 48).map(|icon| icon.is_some()));
    });

    let outcome = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("the loader must return instead of blocking on a FIFO");

    match outcome {
        Err(IconError::Unreadable { detail, .. }) => {
            assert!(
                detail.contains("not an ordinary file"),
                "the refusal names what was wrong with it, got {detail:?}"
            );
        }
        other => panic!("expected an unreadable refusal, got {other:?}"),
    }
}
