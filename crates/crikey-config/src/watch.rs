//! Noticing that a configuration file changed.
//!
//! Deliberately a stat-based poll rather than a filesystem-notification API.
//! Three reasons: a notification API is per-platform, and this crate must stay
//! platform-independent (spec 5.3); the set of files is tiny and fixed, so a
//! poll costs a handful of `stat` calls at whatever interval the launcher's event
//! loop already runs at; and the interesting case — a file that does not exist
//! yet, in a directory that does not exist yet — is exactly where watch APIs are
//! least uniform.
//!
//! The bound this accepts in exchange is honest and stated: a change is noticed
//! on the next poll, not instantly. A same-length rewrite inside one filesystem
//! timestamp tick is NOT excused by that bound, because polling cannot rescue
//! it — neither the timestamp nor the length would ever move again. So a file
//! stamp also carries a fingerprint of the file's content, and only a rewrite
//! that is same-length, same-timestamp AND hash-colliding past the
//! fingerprint's one-megabyte cap stays invisible.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What a watched path looked like when the watch was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stamp {
    /// The path does not exist. Still watched: the first `config.toml` a user
    /// writes is a configuration change, and a watch that only knew about
    /// existing files would never see it.
    Missing,
    File {
        modified: Option<SystemTime>,
        length: u64,
        /// A hash of the file's first `FINGERPRINT_LIMIT` bytes, or `None` when
        /// the content could not be read. Present because `modified` and `length`
        /// alone cannot distinguish a same-length edit made inside one
        /// filesystem timestamp tick, and no later poll ever could either.
        fingerprint: Option<u64>,
    },
    /// A directory is stamped by its own timestamp AND its entry count, because
    /// adding a per-plugin file changes one or the other on every filesystem this
    /// runs on.
    Directory {
        modified: Option<SystemTime>,
        entries: usize,
    },
}

impl Stamp {
    fn of(path: &Path) -> Self {
        let Ok(metadata) = std::fs::metadata(path) else {
            return Self::Missing;
        };
        let modified = metadata.modified().ok();
        if metadata.is_dir() {
            let entries = std::fs::read_dir(path)
                .map(|entries| entries.filter(Result::is_ok).count())
                .unwrap_or(0);
            Self::Directory { modified, entries }
        } else {
            Self::File {
                modified,
                length: metadata.len(),
                fingerprint: fingerprint(path),
            }
        }
    }
}

/// Cap on the bytes hashed into a file stamp.
///
/// A configuration file is a handful of kilobytes, so this never binds in
/// practice; it exists so a watched path replaced by something enormous costs one
/// bounded read per poll rather than an unbounded one.
const FINGERPRINT_LIMIT: u64 = 1 << 20;

/// Hashes the first `FINGERPRINT_LIMIT` bytes of `path`.
///
/// `None` when the content cannot be read, which is not the same as an empty
/// file: a stamp with no fingerprint falls back to the timestamp and length, and
/// stamping the unreadable case as some fixed value would make an unreadable file
/// and a specific readable one compare equal.
///
/// The hasher only has to be stable for the life of one process — stamps are
/// compared against stamps this process took — so the standard library's default
/// hasher is enough and this crate needs no hashing dependency.
fn fingerprint(path: &Path) -> Option<u64> {
    use std::hash::Hasher;
    use std::io::Read;

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0u8; 8192];
    let mut remaining = FINGERPRINT_LIMIT;
    while remaining > 0 {
        let wanted = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(buffer.len()));
        match file.read(&mut buffer[..wanted]) {
            Ok(0) => break,
            Ok(read) => {
                hasher.write(&buffer[..read]);
                remaining -= read as u64;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(hasher.finish())
}

/// A snapshot of every file a [`crate::ConfigStore`] was read from.
#[derive(Debug, Clone)]
pub struct ConfigSourceWatch {
    entries: Vec<(PathBuf, Stamp)>,
}

impl ConfigSourceWatch {
    /// Stamps every path in `paths`.
    pub(crate) fn over(paths: &[PathBuf]) -> Self {
        Self {
            entries: paths.iter().map(|path| (path.clone(), Stamp::of(path))).collect(),
        }
    }

    /// Whether any watched path differs from when the watch was taken.
    ///
    /// Takes `&self` and re-stats on each call, so a caller cannot accidentally
    /// consume the answer: the launcher asks, and only replaces the watch once it
    /// has actually reloaded.
    pub fn changed(&self) -> bool {
        self.entries.iter().any(|(path, stamp)| Stamp::of(path) != *stamp)
    }

    /// Every watched path, for a diagnostic that names what is being watched.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.entries.iter().map(|(path, _)| path.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private temporary directory, removed on drop.
    ///
    /// Hand-rolled because this crate has no test-only dependency and one
    /// directory per test does not justify adding one.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "crikey-config-watch-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a temporary directory can be created");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_unchanged_set_of_files_reports_no_change() {
        let temp = TempDir::new("unchanged");
        let file = temp.0.join("config.toml");
        std::fs::write(&file, "a = 1\n").expect("write");
        let watch = ConfigSourceWatch::over(&[file]);
        assert!(!watch.changed());
    }

    #[test]
    fn creating_a_watched_file_that_did_not_exist_is_a_change() {
        let temp = TempDir::new("created");
        let file = temp.0.join("config.toml");
        let watch = ConfigSourceWatch::over(&[file.clone()]);
        assert!(!watch.changed(), "the file is absent and was absent");
        std::fs::write(&file, "a = 1\n").expect("write");
        assert!(watch.changed(), "the first config.toml a user writes is a change");
    }

    #[test]
    fn editing_a_watched_file_is_a_change() {
        let temp = TempDir::new("edited");
        let file = temp.0.join("config.toml");
        std::fs::write(&file, "a = 1\n").expect("write");
        let watch = ConfigSourceWatch::over(&[file.clone()]);
        std::fs::write(&file, "a = 1\nb = 2\n").expect("write");
        assert!(watch.changed());
    }

    #[test]
    fn an_equal_length_edit_is_a_change_even_when_mtime_is_restored() {
        let temp = TempDir::new("equal-length");
        let file = temp.0.join("config.toml");
        std::fs::write(&file, "a = 1\n").expect("write");
        let original = std::fs::metadata(&file)
            .expect("metadata")
            .modified()
            .expect("mtime");
        let watch = ConfigSourceWatch::over(std::slice::from_ref(&file));
        std::fs::write(&file, "b = 2\n").expect("same-length rewrite");
        std::fs::File::open(&file)
            .expect("open")
            .set_times(std::fs::FileTimes::new().set_modified(original))
            .expect("restore mtime");
        assert!(
            watch.changed(),
            "content changed despite identical mtime and length"
        );
    }

    #[test]
    fn deleting_a_watched_file_is_a_change() {
        let temp = TempDir::new("deleted");
        let file = temp.0.join("config.toml");
        std::fs::write(&file, "a = 1\n").expect("write");
        let watch = ConfigSourceWatch::over(&[file.clone()]);
        std::fs::remove_file(&file).expect("remove");
        assert!(watch.changed());
    }

    #[test]
    fn adding_a_file_to_a_watched_directory_is_a_change() {
        let temp = TempDir::new("directory");
        let directory = temp.0.join("plugins");
        std::fs::create_dir_all(&directory).expect("create");
        let watch = ConfigSourceWatch::over(&[directory.clone()]);
        assert!(!watch.changed());
        std::fs::write(directory.join("modern.example.toml"), "x = 1\n").expect("write");
        assert!(
            watch.changed(),
            "a new per-plugin settings file must be picked up"
        );
    }
}
