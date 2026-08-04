//! The decoded-icon cache (spec 22.1, 22.4).
//!
//! Decoding is not expensive once; it is expensive on every keystroke of every
//! session, for every row, forever. A rasterised SVG is the worst case -- parse
//! an XML document, build a path tree, render it -- and a launcher that draws
//! twenty rows per frame cannot pay for that twice.
//!
//! Entries live under [`StandardDirectories::cache_dir`], which is the directory
//! whose whole contract is that deleting it costs nothing (spec 18.3). Nothing
//! here is authoritative: every failure path -- unwritable directory, torn
//! entry, stale entry -- degrades to decoding the source again.
//!
//! # Which spec 22.4 invalidators apply, and how
//!
//! Of the nine listed invalidators, four can reach an icon, and each is a field
//! this format compares rather than a signal somebody has to remember to send:
//!
//! * **Filesystem events.** The entry records the source file's length, its
//!   modification time and a hash of its bytes. A theme package that replaced
//!   `firefox.png` in place changes at least the hash -- even a restore that
//!   preserves both the length and the timestamp, which the metadata alone
//!   cannot tell from the original. This is what makes the cache safe without a
//!   file watcher: the check is performed on the read that would otherwise use
//!   the stale pixels.
//! * **Application-installation changes.** The same mechanism, one level up: an
//!   install or removal changes which *file* a reference resolves to, so the
//!   located path changes and the entry the new path hashes to is a different
//!   entry.
//! * **Platform-backend changes.** The backend name is part of both the entry
//!   name and the entry body. An icon reference means different things to
//!   different backends -- a themed name to the Linux one, a shortcut icon
//!   location to the Windows one -- so pixels cached by one must never be read
//!   by another.
//! * **Schema-version changes.** [`ICON_CACHE_SCHEMA_VERSION`] is compared
//!   before any field is trusted, so a build that changes the layout, the pixel
//!   convention, or the size limits ignores every entry the previous one wrote
//!   instead of misreading it.
//!
//! The other five do not apply: an icon has no manifest, no plugin, no
//! configuration input, and no expiry beyond its source file changing.
//!
//! [`StandardDirectories::cache_dir`]: crate::StandardDirectories::cache_dir

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

use super::{check_dimensions, IconImage, MAX_ICON_PIXELS};
use crate::StandardDirectories;

/// The layout version of one cache entry.
///
/// Bump this whenever the entry body, the pixel convention, or the spec 11.7
/// limits change: an entry written by a build that disagrees about any of them
/// is not stale data, it is data this build cannot interpret.
pub const ICON_CACHE_SCHEMA_VERSION: u32 = 2;

/// Marks a file as one of ours before a single field of it is trusted.
const MAGIC: &[u8; 8] = b"CRIKICON";

/// The subdirectory of the cache root this cache owns, so that a sweeper -- or
/// a person -- can delete the icon cache alone.
const CACHE_SUBDIRECTORY: &str = "icons";

/// What a source file looked like when its pixels were cached.
///
/// Length, modification time and a hash of the contents. The metadata alone is
/// not proof that the pixels are current: an in-place edit that preserves the
/// size keeps the length, a restored backup or an `rsync --archive` copy keeps
/// the timestamp, and a restore that does both -- the ordinary case for an
/// archive extraction or `cp -p` -- keeps the pair. The content hash is the
/// only field a replacement cannot leave equal, so it is what decides.
///
/// The hash is FNV-1a rather than a cryptographic digest for the same reason
/// [`IconImage::content_id`] is: nothing here is a trust boundary. The source
/// file is read on every load anyway -- the cache saves the *decode*, which for
/// a rasterised SVG is what actually costs -- so hashing it is a pass over
/// bytes already in hand.
///
/// `modified` is `None` on a filesystem that reports no modification time. Such
/// a source is never cached: the timestamp is what bounds how long a hash
/// collision could serve the wrong pixels, and serving the wrong icon forever
/// is worse than decoding every time.
///
/// [`IconImage::content_id`]: super::IconImage::content_id
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub len: u64,
    pub modified: Option<(i64, u32)>,
    pub content: u64,
}

impl SourceFingerprint {
    /// Reads `path` and fingerprints its bytes.
    pub fn probe(path: &Path) -> Result<Self, String> {
        let contents = fs::read(path).map_err(|error| error.to_string())?;
        Self::probe_with_contents(path, &contents)
    }

    /// Fingerprints bytes read from `path`.
    pub fn probe_with_contents(path: &Path, contents: &[u8]) -> Result<Self, String> {
        let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
        let modified = metadata
            .modified()
            .ok()
            .map(|modified| match modified.duration_since(UNIX_EPOCH) {
                Ok(since) => (since.as_secs() as i64, since.subsec_nanos()),
                Err(before) => {
                    let before = before.duration();
                    (-(before.as_secs() as i64), before.subsec_nanos())
                }
            });
        Ok(Self {
            len: contents.len() as u64,
            modified,
            content: fnv1a(contents),
        })
    }

    /// Appends the fingerprint to an entry body.
    fn write_into(&self, entry: &mut Vec<u8>) {
        let (secs, nanos) = self.modified.unwrap_or((0, 0));
        entry.extend_from_slice(&self.len.to_le_bytes());
        entry.extend_from_slice(&secs.to_le_bytes());
        entry.extend_from_slice(&nanos.to_le_bytes());
        entry.extend_from_slice(&self.content.to_le_bytes());
    }
}

/// FNV-1a over `bytes`.
const fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(PRIME);
        index += 1;
    }
    hash
}

/// Everything that decides whether cached pixels answer this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconCacheKey {
    /// The reference as the item carried it.
    pub reference: String,
    /// The edge that was requested, because a vector source renders to it and
    /// a container picks a frame by it.
    pub size: u32,
    /// The file the reference resolved to.
    pub source: PathBuf,
    /// What that file looked like.
    pub fingerprint: SourceFingerprint,
}

/// Decoded icon pixels on disk, under the cache directory (spec 22.1).
#[derive(Debug)]
pub struct IconCache {
    root: PathBuf,
    backend: &'static str,
}

impl IconCache {
    /// The cache under `directories`, owned by the named platform backend.
    pub fn new(directories: &StandardDirectories, backend: &'static str) -> Self {
        Self {
            root: directories.cache_dir().join(CACHE_SUBDIRECTORY),
            backend,
        }
    }

    /// The pixels cached for `key`, or `None` for anything else at all.
    ///
    /// "Anything else" is deliberately one answer: a missing entry, an entry
    /// from another schema version or backend, an entry whose source has since
    /// changed, and a half-written entry all mean the same thing to the caller,
    /// which is "decode it again". A cache read never fails the icon.
    pub fn load(&self, key: &IconCacheKey) -> Option<IconImage> {
        key.fingerprint.modified?;
        let bytes = read_entry(&self.entry_path(key))?;
        let mut reader = EntryReader::new(&bytes);

        if reader.take(MAGIC.len())? != MAGIC {
            return None;
        }
        if reader.u32()? != ICON_CACHE_SCHEMA_VERSION {
            return None;
        }
        if reader.text()? != self.backend {
            return None;
        }
        if reader.text()? != key.reference {
            return None;
        }
        if reader.u32()? != key.size {
            return None;
        }
        let len = reader.u64()?;
        let secs = reader.i64()?;
        let nanos = reader.u32()?;
        let content = reader.u64()?;
        if key.fingerprint.len != len
            || key.fingerprint.modified != Some((secs, nanos))
            || key.fingerprint.content != content
        {
            return None;
        }

        let width = reader.u32()?;
        let height = reader.u32()?;
        check_dimensions(&key.reference, width, height).ok()?;
        let rgba = reader.take((width as usize) * (height as usize) * 4)?.to_vec();
        // The trailing-bytes check is not pedantry: an entry longer than its own
        // dimensions describe is a torn write over a larger predecessor, whose
        // prefix is a valid header for pixels that are not all there.
        if !reader.is_empty() {
            return None;
        }
        IconImage::new(&key.reference, width, height, rgba).ok()
    }

    /// Records `image` as the answer to `key`, best effort.
    ///
    /// The entry is written to a unique temporary name and renamed into place,
    /// so a reader never observes a partial one and two processes caching the
    /// same icon at once cannot interleave their bytes. Every failure is
    /// swallowed: a read-only or full cache directory must cost a decode, not an
    /// icon.
    pub fn store(&self, key: &IconCacheKey, image: &IconImage) {
        if key.fingerprint.modified.is_none() {
            return;
        }
        if fs::create_dir_all(&self.root).is_err() {
            return;
        }

        let mut entry = Vec::with_capacity(image.rgba().len() + 128);
        entry.extend_from_slice(MAGIC);
        entry.extend_from_slice(&ICON_CACHE_SCHEMA_VERSION.to_le_bytes());
        write_text(&mut entry, self.backend);
        write_text(&mut entry, &key.reference);
        entry.extend_from_slice(&key.size.to_le_bytes());
        key.fingerprint.write_into(&mut entry);
        entry.extend_from_slice(&image.width().to_le_bytes());
        entry.extend_from_slice(&image.height().to_le_bytes());
        entry.extend_from_slice(image.rgba());

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let staging = self.root.join(format!(
            "{}.{}.{}.staging",
            self.entry_name(key),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if fs::write(&staging, &entry).is_err() {
            let _ = fs::remove_file(&staging);
            return;
        }
        if fs::rename(&staging, self.entry_path(key)).is_err() {
            let _ = fs::remove_file(&staging);
        }
    }

    fn entry_path(&self, key: &IconCacheKey) -> PathBuf {
        self.root.join(format!("{}.icon", self.entry_name(key)))
    }

    /// The entry name: a hash of everything that identifies the request.
    ///
    /// A hash rather than the reference itself because a reference is an
    /// arbitrary string -- an absolute path, on Unix an arbitrary byte string --
    /// and a filename derived from one would be neither valid nor unique. The
    /// body records the reference in full, so the entry is only used when it
    /// really is the one asked for.
    fn entry_name(&self, key: &IconCacheKey) -> String {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut hash = OFFSET;
        let mut mix = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(PRIME);
            }
        };
        mix(&ICON_CACHE_SCHEMA_VERSION.to_le_bytes());
        mix(self.backend.as_bytes());
        mix(key.reference.as_bytes());
        mix(&key.size.to_le_bytes());
        mix(path_bytes(&key.source).as_ref());
        format!("{hash:016x}")
    }
}

/// The bytes of a path, without a lossy conversion.
///
/// Only ever hashed, never reconstructed, so the Windows arm may spell UTF-16
/// code units little-endian rather than round-tripping them: two distinct paths
/// must land on distinct bytes, and nothing here needs the inverse.
#[cfg(unix)]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;

    std::borrow::Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(windows)]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::windows::ffi::OsStrExt;

    let mut bytes = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::borrow::Cow::Owned(bytes)
}

fn write_text(entry: &mut Vec<u8>, text: &str) {
    entry.extend_from_slice(&(text.len() as u32).to_le_bytes());
    entry.extend_from_slice(text.as_bytes());
}

/// Reads a cache entry, capped at the largest one this build can have written.
///
/// The cap matters because the cache directory is an ordinary directory: a user,
/// a backup tool or a hostile package can put a gigabyte there under a name that
/// hashes correctly, and a cache read must not be a way to allocate one.
fn read_entry(path: &Path) -> Option<Vec<u8>> {
    /// Header plus the largest pixel buffer the spec 11.7 limits permit, plus
    /// one byte so that "too long" is distinguishable from "exactly at the cap".
    const CAP: u64 = 4096 + MAX_ICON_PIXELS * 4 + 1;

    // Through the same non-blocking open the icon readers use: the cache
    // directory is an ordinary directory, so an entry can be a FIFO exactly as an
    // icon can.
    let file = super::open_for_read(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(CAP).read_to_end(&mut bytes).ok()?;
    ((bytes.len() as u64) < CAP).then_some(bytes)
}

/// A cursor over an entry that returns `None` instead of panicking on a short
/// read, so a truncated entry is a cache miss rather than a crash.
#[derive(Debug)]
struct EntryReader<'a> {
    rest: &'a [u8],
}

impl<'a> EntryReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.rest.split_at_checked(count)?;
        self.rest = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn text(&mut self) -> Option<&'a str> {
        let length = self.u32()? as usize;
        std::str::from_utf8(self.take(length)?).ok()
    }

    fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }
}
