//! Icons: resolving an item's `icon_reference` to pixels (spec 6.4, 18.1).
//!
//! An item carries an `icon_reference`, which is whatever the producing plugin
//! or backend can say about its icon: a themed name (`firefox`), an absolute
//! path, a Windows icon location, a bundle resource. Nothing downstream can
//! interpret that string, because what it means depends on the platform that
//! wrote it -- so this module splits the job in two.
//!
//! * [`IconSource`] is the per-platform half: *which file* holds the icon for
//!   this reference at this size. That is the only part a backend has to
//!   implement, and it is the only part that needs to know about XDG icon
//!   themes, shortcut icon locations or bundle layouts.
//! * [`IconLoader`] is the shared half: read the file under the spec 11.7
//!   payload limit, decode it, and cache the pixels (spec 22.1). It is
//!   identical on every platform, so it lives here and every host can test it.
//!
//! [`IconProvider`] is what the composition root consumes. Splitting it from
//! [`IconSource`] is not ceremony: the loader is what turns a located file into
//! a checked, cached [`IconImage`], and a backend that implemented the provider
//! directly would be re-implementing -- and would be free to forget -- the size
//! limit and the cache.
//!
//! # Why a size limit is not optional
//!
//! Icon files come from third-party packages, downloaded themes and shortcuts
//! any user can drop into their own data directory. A 900-byte PNG can declare
//! a 30000x30000 image, which is 3.6 GiB of RGBA. Every entry point here
//! therefore checks a declared size *before* allocating for it, and refuses
//! rather than trying: [`MAX_ICON_PAYLOAD_BYTES`] bounds what is read from disk
//! and [`MAX_ICON_EDGE`]/[`MAX_ICON_PIXELS`] bound what is decoded (spec 11.7).

use std::{
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

mod cache;
mod decode;

pub use cache::{IconCache, IconCacheKey, SourceFingerprint, ICON_CACHE_SCHEMA_VERSION};
pub use decode::{decode_icon, IconFormat};

use crate::StandardDirectories;

/// The largest encoded icon file this build will read (spec 11.7).
///
/// Four mebibytes is well above every real icon -- a 512x512 PNG is tens of
/// kilobytes, the largest `.icns` Apple ships is under two megabytes -- and far
/// below anything that hurts to read. A file larger than this is refused by
/// name rather than truncated, because half an icon is not an icon.
pub const MAX_ICON_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024;

/// The largest decoded edge, in pixels (spec 11.7).
///
/// Bounds the *declared* dimensions, so a compressed bomb is refused from its
/// header instead of after the allocation it asked for.
pub const MAX_ICON_EDGE: u32 = 1024;

/// The largest decoded pixel count (spec 11.7).
///
/// Checked in addition to [`MAX_ICON_EDGE`] because a limit on each edge
/// separately still admits shapes no icon has, and the product is what decides
/// the allocation.
pub const MAX_ICON_PIXELS: u64 = (MAX_ICON_EDGE as u64) * (MAX_ICON_EDGE as u64);

/// The icon edge the launcher asks for, in logical pixels.
///
/// A request is a hint: an icon theme answers with the closest size it has and
/// a vector source renders at exactly this edge, so the returned [`IconImage`]
/// may be larger or smaller and the renderer scales it.
pub const DEFAULT_ICON_SIZE: u32 = 48;

/// Decoded icon pixels: tightly packed, row-major, straight (not
/// premultiplied) RGBA8.
///
/// One convention, stated once, because the two decoders disagree by default:
/// PNG is straight alpha and a rasterised SVG is premultiplied, and a renderer
/// handed both without being told which is which draws dark halos around every
/// vector icon.
#[derive(Clone, PartialEq, Eq)]
pub struct IconImage {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    content_id: u64,
}

/// Prints the shape and identity, never the pixels: a 48x48 icon is 9216 bytes,
/// and a `{:?}` of a row model containing one is otherwise unreadable.
impl fmt::Debug for IconImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IconImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("content_id", &self.content_id)
            .finish()
    }
}

impl IconImage {
    /// Wraps decoded pixels, checking that they describe the image they claim.
    ///
    /// # Errors
    ///
    /// [`IconError::Malformed`] when `rgba` is not exactly `width * height * 4`
    /// bytes or either edge is zero, and [`IconError::TooLarge`] past the
    /// spec 11.7 bounds. A decoder that miscounts a stride must fail here
    /// rather than hand the renderer a buffer it will read past.
    pub fn new(reference: &str, width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, IconError> {
        check_dimensions(reference, width, height)?;
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() != expected {
            return Err(IconError::Malformed {
                reference: reference.to_owned(),
                detail: format!("{width}x{height} needs {expected} RGBA bytes, got {}", rgba.len()),
            });
        }
        let content_id = content_id(width, height, &rgba);
        Ok(Self {
            width,
            height,
            rgba,
            content_id,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The pixels: `width * height * 4` bytes of straight RGBA8.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// A content-derived identity, stable across processes and across a cache
    /// round trip.
    ///
    /// The renderer keys its GPU texture cache on this rather than on the icon
    /// reference: a reference is not unique to one image -- the theme behind it
    /// can be replaced, and two references routinely resolve to the same file --
    /// while hashing the pixels afresh every frame would cost more than the
    /// upload it avoids.
    pub fn content_id(&self) -> u64 {
        self.content_id
    }
}

/// FNV-1a over the dimensions and the pixels.
///
/// Not a cryptographic digest and not required to be one: it identifies an image
/// inside one process's texture cache, where the cost of a collision is drawing
/// the wrong 48x48 square. It is deliberately deterministic, so an image loaded
/// from the disk cache gets the identity the freshly decoded one had and the
/// texture is reused across restarts.
fn content_id(width: u32, height: u32, rgba: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for byte in width
        .to_le_bytes()
        .iter()
        .chain(height.to_le_bytes().iter())
        .chain(rgba.iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Rejects dimensions past the spec 11.7 bounds, before anything is allocated
/// for them.
pub(crate) fn check_dimensions(reference: &str, width: u32, height: u32) -> Result<(), IconError> {
    if width == 0 || height == 0 {
        return Err(IconError::Malformed {
            reference: reference.to_owned(),
            detail: format!("an icon cannot be {width}x{height}"),
        });
    }
    if width > MAX_ICON_EDGE || height > MAX_ICON_EDGE {
        return Err(IconError::TooLarge {
            reference: reference.to_owned(),
            limit: u64::from(MAX_ICON_EDGE),
            found: u64::from(width.max(height)),
            what: "icon edge in pixels",
        });
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_ICON_PIXELS {
        return Err(IconError::TooLarge {
            reference: reference.to_owned(),
            limit: MAX_ICON_PIXELS,
            found: pixels,
            what: "decoded icon pixels",
        });
    }
    Ok(())
}

/// Why an icon reference produced no pixels.
///
/// "No icon here" is not one of these: that is `Ok(None)`, and it is the
/// ordinary case for an item whose plugin named no icon or whose theme has
/// nothing under that name. These are the cases where something *was* found and
/// could not be used, which a launcher reports once and then draws the row
/// without an icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconError {
    /// A declared or observed size exceeds a spec 11.7 limit. `what` names which
    /// limit, so the message says whether the file or the image was the problem.
    TooLarge {
        reference: String,
        limit: u64,
        found: u64,
        what: &'static str,
    },
    /// The bytes are a format this build does not decode.
    UnsupportedFormat { reference: String, detail: String },
    /// The bytes claim a format they then violate.
    Malformed { reference: String, detail: String },
    /// The located file could not be read.
    Unreadable { reference: String, detail: String },
}

impl fmt::Display for IconError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge {
                reference,
                limit,
                found,
                what,
            } => write!(
                formatter,
                "icon {reference:?} exceeds the {what} limit of {limit}: found {found}"
            ),
            Self::UnsupportedFormat { reference, detail } => {
                write!(
                    formatter,
                    "icon {reference:?} is in an undecodable format: {detail}"
                )
            }
            Self::Malformed { reference, detail } => {
                write!(formatter, "icon {reference:?} is malformed: {detail}")
            }
            Self::Unreadable { reference, detail } => {
                write!(formatter, "icon {reference:?} could not be read: {detail}")
            }
        }
    }
}

impl std::error::Error for IconError {}

/// The per-platform half of icon resolution: which file holds this reference.
///
/// Implementations answer from the filesystem and from platform conventions
/// only -- an XDG theme search, a shortcut's icon location, a bundle's
/// `Resources` directory. They neither read nor decode the file: that is
/// [`IconLoader`]'s job, and it is where the payload limit and the cache live.
///
/// `None` means "this platform knows of no icon file for that reference", which
/// covers the common cases of a themed name the installed themes do not carry
/// and a path that no longer exists.
pub trait IconSource {
    /// Locates the icon file for `reference`, preferring one whose natural edge
    /// is `size` pixels.
    fn locate(&self, reference: &str, size: u32) -> Option<PathBuf>;
}

/// What the composition root asks for an icon.
///
/// `Ok(None)` is "no icon for this item", which is normal and silent.
pub trait IconProvider {
    fn load(&self, reference: &str, size: u32) -> Result<Option<IconImage>, IconError>;
}

#[derive(Debug, Clone)]
struct RecentIcon {
    path: PathBuf,
    size: u32,
    modified: Option<SystemTime>,
    len: u64,
    image: IconImage,
}

/// Reads, size-checks, decodes and caches what an [`IconSource`] locates.
///
/// The backend-independent half of [`IconProvider`], so that every platform
/// enforces the same spec 11.7 limits and shares one cache format. The cache is
/// optional: [`IconLoader::new`] decodes on every call, which is what a
/// decoder test wants, and [`IconLoader::caching`] adds the on-disk cache the
/// launcher runs with.
#[derive(Debug)]
pub struct IconLoader<S> {
    source: S,
    cache: Option<IconCache>,
    recent: Mutex<Option<RecentIcon>>,
}

impl<S: IconSource> IconLoader<S> {
    /// A loader that decodes every call unless the source metadata and path
    /// match the immediately preceding successful load.
    pub fn new(source: S) -> Self {
        Self {
            source,
            cache: None,
            recent: Mutex::new(None),
        }
    }

    /// A loader that keeps decoded pixels under [`StandardDirectories::cache_dir`].
    pub fn caching(source: S, backend: &'static str, directories: &StandardDirectories) -> Self {
        Self {
            source,
            cache: Some(IconCache::new(directories, backend)),
            recent: Mutex::new(None),
        }
    }

    fn load_located(&self, reference: &str, size: u32, path: &Path) -> Result<IconImage, IconError> {
        let bytes = match read_capped(reference, path, MAX_ICON_PAYLOAD_BYTES) {
            Ok(bytes) => bytes,
            Err(error) => {
                if let Ok(metadata) = fs::metadata(path) {
                    let recent = self
                        .recent
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    if let Some(recent) = recent {
                        if recent.path == path
                            && recent.size == size
                            && recent.len == metadata.len()
                            && recent.modified == metadata.modified().ok()
                        {
                            return Ok(recent.image);
                        }
                    }
                }
                return Err(error);
            }
        };
        let fingerprint =
            SourceFingerprint::probe_with_contents(path, &bytes).map_err(|detail| IconError::Unreadable {
                reference: reference.to_owned(),
                detail,
            })?;

        let key = IconCacheKey {
            reference: reference.to_owned(),
            size,
            source: path.to_path_buf(),
            fingerprint,
        };
        if let Some(cache) = &self.cache {
            if let Some(image) = cache.load(&key) {
                if let Ok(metadata) = fs::metadata(path) {
                    *self
                        .recent
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(RecentIcon {
                        path: path.to_path_buf(),
                        size,
                        modified: metadata.modified().ok(),
                        len: metadata.len(),
                        image: image.clone(),
                    });
                }
                return Ok(image);
            }
        }

        let image = decode_icon(reference, &bytes, size)?;
        if let Some(cache) = &self.cache {
            cache.store(&key, &image);
        }
        if let Ok(metadata) = fs::metadata(path) {
            *self
                .recent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(RecentIcon {
                path: path.to_path_buf(),
                size,
                modified: metadata.modified().ok(),
                len: metadata.len(),
                image: image.clone(),
            });
        }
        Ok(image)
    }
}

impl<S: IconSource> IconProvider for IconLoader<S> {
    fn load(&self, reference: &str, size: u32) -> Result<Option<IconImage>, IconError> {
        let Some(path) = self.source.locate(reference, size) else {
            return Ok(None);
        };
        self.load_located(reference, size, &path).map(Some)
    }
}

/// Reads at most `limit` bytes, refusing a file that has more.
///
/// The reader is capped one byte past the limit so that "reached the cap" and
/// "is exactly at the cap" are distinguishable: a file that grew between the
/// stat and the read is refused instead of being decoded from a truncated
/// prefix.
///
/// The open goes through [`open_for_read`] and the metadata comes from the open
/// descriptor rather than from the path: an [`IconSource`] hands over a path it
/// checked a moment ago, and what is at that path when the read happens is not
/// something either of them controls.
fn read_capped(reference: &str, path: &Path, limit: u64) -> Result<Vec<u8>, IconError> {
    let unreadable = |detail: String| IconError::Unreadable {
        reference: reference.to_owned(),
        detail,
    };
    let file = open_for_read(path).map_err(|error| unreadable(error.to_string()))?;
    let metadata = file.metadata().map_err(|error| unreadable(error.to_string()))?;
    if !metadata.is_file() {
        return Err(unreadable(format!("{} is not an ordinary file", path.display())));
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(limit) as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unreadable(error.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(IconError::TooLarge {
            reference: reference.to_owned(),
            limit,
            found: bytes.len() as u64,
            what: "encoded icon payload in bytes",
        });
    }
    Ok(bytes)
}

/// Opens a file for reading without ever waiting for it.
///
/// Icon files are named by desktop entries, shell links and bundle plists, all of
/// which are written by third parties, and they live in directories any user can
/// write to. Two flags follow from that, on every Unix:
///
/// * `O_NONBLOCK` makes the open of a FIFO return immediately instead of blocking
///   until a writer appears. Without it, a named pipe planted where an icon is
///   expected hangs icon loading -- and with it the row that asked -- for the rest
///   of the session.
/// * `O_CLOEXEC` keeps the descriptor from being inherited by a plugin process
///   the launcher spawns later.
///
/// Symlinks are still followed, deliberately: distributions do ship icons as
/// links. What the caller checks is the *open descriptor*, so a link to a device
/// or a FIFO is refused by the `is_file` check rather than by refusing links.
///
/// On a non-Unix target there is no FIFO to block on and `CreateFile` is already
/// close-on-exec by default, so a plain open is the whole of it.
#[cfg(unix)]
pub(crate) fn open_for_read(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
pub(crate) fn open_for_read(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

/// Locates icons that a reference already names by absolute path.
///
/// The whole of icon resolution on a platform whose references are paths, and
/// the tail of it on one whose references are not: a themed name that resolved
/// to a file, a bundle resource, and a shortcut's icon location with its index
/// stripped all end here.
///
/// A relative path is refused rather than joined onto the launcher's working
/// directory: a catalog entry is not a shell, and `icons/app.png` relative to
/// wherever the process happens to have been started names a different file on
/// every run. An extension this build cannot decode is refused too, so an
/// `app.exe` icon location reports "no icon" instead of feeding a PE image to a
/// PNG decoder.
#[derive(Debug, Default, Clone, Copy)]
pub struct PathIconSource;

impl IconSource for PathIconSource {
    fn locate(&self, reference: &str, _size: u32) -> Option<PathBuf> {
        let path = Path::new(reference);
        if !path.is_absolute() || IconFormat::from_extension(path).is_none() {
            return None;
        }
        path.is_file().then(|| path.to_path_buf())
    }
}
