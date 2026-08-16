//! Searching the user's files and folders by name on Linux (spec 18.1, 18.2).
//!
//! # Why this looks nothing like the Windows or macOS backend
//!
//! Linux has no index every session is guaranteed to have. There is no MFT to
//! read and no Spotlight to ask: a desktop may run a tracker daemon, a server
//! may run `updatedb` from a timer, and a container may have neither. So this
//! backend is built the other way round from a platform with an index -- it
//! owns a mechanism that always works and treats an index as an optimisation
//! it may or may not find at run time.
//!
//! * A **bounded walk** over the configured roots (`$HOME` by default) is the
//!   floor. It needs nothing installed, it costs exactly the time the caller
//!   lends it, and it sees files created a second ago. Breadth-first rather
//!   than depth-first because the deadline usually expires mid-walk, and when
//!   it does the user would rather have `~/notes.md` than the deepest corner
//!   of one subtree (spec 18.1).
//! * **`plocate`**, when the binary is installed, answers from a trigram index
//!   over path names and is orders of magnitude faster than any walk. It is
//!   only ever as fresh as the last `updatedb`, so an answer from it is
//!   [`FileSearchCoverage::Partial`] -- real hits, but a file created since the
//!   last index run is invisible to it.
//!
//! The delegation goes through the *binary*, never through the database file.
//! `plocate`'s on-disk format is versioned with its implementation and is
//! documented nowhere as an interface; the command line is the stable API, and
//! it is also the only path that respects the pruning `updatedb.conf` was
//! configured with.
//!
//! # The deadline
//!
//! Search runs on a keystroke, so [`FileSearchQuery::deadline`] is a promise
//! (see the trait's module documentation). The walk therefore checks the clock
//! before every directory and every [`DEADLINE_CHECK_STRIDE`] entries, and the
//! delegation gets *half* the remaining budget so that a `plocate` that hangs
//! on a cold cache still leaves a walk something to answer with. A search that
//! runs out of time returns the hits it has with
//! [`FileSearchCoverage::Deadline`]; it is not an error, and it is the ordinary
//! case on the first keystroke over a large home directory.
//!
//! # What is deliberately not done here
//!
//! No fuzzy matching and no scoring: the host's matcher ranks whatever comes
//! back, and a second scoring pass here would fight it. Matching is a
//! case-insensitive substring test on the basename only, which is what the
//! [`FileHit`] split of `name` from `path` exists for. No content search
//! either, per the trait contract -- names and locations only.

use std::collections::VecDeque;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, FileType};
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crikey_core::{PlatformPath, Result};
use crikey_platform::{
    FileHit, FileKind, FileSearchCoverage, FileSearchQuery, FileSearchResults, FileSearchService,
    MAX_FILE_HITS,
};

/// Directory names the walk never descends into, hidden names aside.
///
/// Both are build or version-control machinery: a `node_modules` tree holds
/// tens of thousands of files nobody launches by name, and descending one
/// spends the whole deadline in a subtree whose hits would be noise. `.git` is
/// already covered by the hidden-directory rule and is listed anyway, because
/// the rule and the intent are different things and a future root layout may
/// hand us a `.git` that is not hidden by name.
pub const EXCLUDED_DIRECTORY_NAMES: &[&str] = &[".git", "node_modules"];

/// Path prefixes that are not files at all.
///
/// `/proc`, `/sys` and `/dev` are kernel interfaces: walking them is unbounded
/// (one directory per process, per thread, per device), reading them can block
/// on hardware, and nothing in them is a file a user launches by name. They are
/// excluded whether they arrive as a configured root or as a symlink target's
/// parent.
pub const EXCLUDED_PREFIXES: &[&str] = &["/proc", "/sys", "/dev"];

/// How many directory entries the walk may inspect between two clock reads.
///
/// The check is not per entry because [`Instant::now`] is a syscall-shaped
/// vDSO call and a large home directory has millions of entries; it is not per
/// directory either, because one directory can hold a hundred thousand entries
/// and the deadline would then be missed by the width of that one directory.
/// A stride bounds the overrun by the cost of 128 `readdir` steps, which is
/// microseconds.
pub const DEADLINE_CHECK_STRIDE: usize = 128;

/// The index-backed helper this backend will delegate to when it is installed.
const LOCATE_BINARY: &str = "plocate";

/// How many index candidates to ask for per hit the caller wants.
///
/// `plocate` searches the whole filesystem while this service answers for its
/// configured roots only, so a share of what it returns is discarded. Asking
/// for a multiple of the limit keeps a query whose matches are mostly outside
/// `$HOME` from coming back nearly empty, and the cap keeps a pathological
/// query (`e`) from making the host decode megabytes of paths it will drop.
const LOCATE_OVERSAMPLE: usize = 4;

/// Upper bound on candidates requested from the index, whatever the limit is.
const LOCATE_MAX_CANDIDATES: usize = 4096;

/// File search over the user's roots, with `plocate` in front of it when the
/// session has it (spec 18.1).
///
/// Construction touches the filesystem only to answer "does this exist": no
/// walk starts and no child process runs until [`FileSearchService::search`] is
/// called, so a service can be built during startup on the main thread.
#[derive(Debug, Clone)]
pub struct FilesystemSearch {
    roots: Vec<PathBuf>,
    locate: Option<PathBuf>,
}

impl FilesystemSearch {
    /// The service for the running user: `$HOME` as the only root, plus
    /// `plocate` if it is on `PATH`.
    ///
    /// `$HOME` alone, rather than the whole filesystem, because a launcher's
    /// file search answers for the user's own documents; `/usr` is covered by
    /// application discovery, and a walk that includes it spends the deadline
    /// on shared libraries. An unset or non-directory `$HOME` yields a service
    /// with no roots, which [`LinuxBackend::file_search`] reports as no service
    /// at all rather than as a search that silently finds nothing.
    ///
    /// [`LinuxBackend::file_search`]: crate::LinuxBackend::file_search
    pub fn for_session() -> Self {
        let roots = match env::var_os("HOME") {
            Some(home) if !home.is_empty() && Path::new(&home).is_dir() => vec![PathBuf::from(home)],
            _ => Vec::new(),
        };

        Self {
            roots,
            locate: locate_binary(),
        }
    }

    /// A service that only ever walks `roots`, in the order given.
    ///
    /// The constructor for an embedder that configures its own roots, and the
    /// one the tests use: no index means no dependency on what the host that
    /// runs the suite happens to have installed.
    pub fn walking(roots: Vec<PathBuf>) -> Self {
        Self { roots, locate: None }
    }

    /// A service that delegates to the `plocate`-compatible binary at
    /// `locate`, falling back to walking `roots` when it cannot answer.
    ///
    /// The binary is not probed here: a path that turns out not to be
    /// executable simply fails to spawn at search time and the walk answers
    /// instead, which is the same path a `plocate` with no database takes.
    pub fn with_locate(roots: Vec<PathBuf>, locate: PathBuf) -> Self {
        Self {
            roots,
            locate: Some(locate),
        }
    }

    /// The roots this service searches, highest precedence first.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether an index stands in front of the walk.
    ///
    /// Read by [`LinuxBackend::capability`] to choose between
    /// [`CapabilityState::Available`] and [`CapabilityState::Partial`]: a walk
    /// sees the live filesystem, an index sees whatever the last `updatedb`
    /// saw, and only the second of those needs a caveat attached to the
    /// session (spec 18.2).
    ///
    /// [`LinuxBackend::capability`]: crate::LinuxBackend::capability
    /// [`CapabilityState::Available`]: crikey_platform::CapabilityState::Available
    /// [`CapabilityState::Partial`]: crikey_platform::CapabilityState::Partial
    pub fn uses_index(&self) -> bool {
        self.locate.is_some()
    }

    /// Hands the query to the index binary, or `None` when it could not answer
    /// and the walk must.
    ///
    /// "Could not answer" deliberately includes *found nothing*. `plocate`
    /// exits non-zero both for an empty result and for a missing database, so
    /// the two cannot be told apart from the outside -- and walking after an
    /// empty index answer is the better behaviour anyway, because the file the
    /// user saved a minute ago is exactly the one the index does not have yet.
    fn delegate_to_locate(
        &self,
        binary: &Path,
        needle: &str,
        limit: usize,
        started: Instant,
        deadline: Duration,
    ) -> Option<Vec<FileHit>> {
        // Half the remaining budget, so that a delegation that stalls still
        // leaves the fallback walk something to answer within.
        let budget = deadline.checked_sub(started.elapsed())? / 2;
        if budget.is_zero() {
            return None;
        }

        let candidates = limit.saturating_mul(LOCATE_OVERSAMPLE).min(LOCATE_MAX_CANDIDATES);
        let mut child = Command::new(binary)
            .arg("--ignore-case")
            .arg("--basename")
            .arg("--null")
            .arg("--limit")
            .arg(candidates.to_string())
            // Everything after `--` is a pattern, so a query that starts with a
            // dash cannot turn into an option. The pattern travels as an argv
            // entry and never through a shell.
            .arg("--")
            .arg(needle)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Discarded on purpose: a missing database is reported through the
            // exit status, and this service must not write another program's
            // diagnostics onto the launcher's stderr.
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // Read on a thread, keep the `Child` here. Reading in this thread would
        // block past the deadline on a stalled index, and moving the child into
        // the thread would leave nothing here to kill it with.
        let mut stdout = child.stdout.take()?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = stdout.read_to_end(&mut bytes);
            let _ = sender.send(read.map(|_| bytes));
        });

        let output = match receiver.recv_timeout(budget) {
            Ok(Ok(bytes)) => bytes,
            // A read error, or a delegation that outlived its budget. Killing
            // it releases the reader thread by closing the pipe, and reaping it
            // here keeps the launcher from collecting zombies over a session.
            Ok(Err(_)) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };

        // The pipe reached EOF, so the child is finishing; this does not block
        // meaningfully. A non-zero status means no usable answer -- see above.
        match child.wait() {
            Ok(status) if status.success() => {}
            _ => return None,
        }

        let hits = self.index_hits(&output, needle, limit);
        if hits.is_empty() {
            return None;
        }

        Some(hits)
    }

    /// Turns NUL-separated index output into hits this service is allowed to
    /// report.
    ///
    /// The filtering is not paranoia, it is equivalence: a hit has to mean the
    /// same thing whichever mechanism produced it, or the same query answers
    /// differently depending on whether `plocate` happens to be installed. So
    /// index candidates are held to the walk's rules -- inside a configured
    /// root, no hidden or excluded directory on the way down, basename really
    /// matches -- and are `lstat`ed, which also drops the entries a stale index
    /// still believes in.
    fn index_hits(&self, output: &[u8], needle: &str, limit: usize) -> Vec<FileHit> {
        let mut hits = Vec::new();

        for candidate in output.split(|byte| *byte == 0) {
            if hits.len() >= limit {
                break;
            }
            if candidate.is_empty() {
                continue;
            }

            let path = PathBuf::from(OsString::from_vec(candidate.to_vec()));
            if !self.is_reportable(&path) {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            // `--basename --ignore-case` folds case by the current locale,
            // which is not necessarily the fold the walk uses. Re-checking here
            // is what keeps the two mechanisms answering the same question.
            if !name_matches(name, needle) {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };

            let kind = if metadata.is_dir() || (metadata.is_symlink() && points_at_directory(&path)) {
                FileKind::Directory
            } else {
                FileKind::File
            };
            hits.push(FileHit {
                name: name.to_string_lossy().into_owned(),
                path: PlatformPath::new(path),
                kind,
                modified_unix_seconds: metadata.modified().ok().map(unix_seconds),
            });
        }

        hits
    }

    /// Whether a path from the index lies inside a root and inside the part of
    /// it the walk would have reached.
    fn is_reportable(&self, path: &Path) -> bool {
        let Some(root) = self.roots.iter().find(|root| path.starts_with(root)) else {
            return false;
        };
        if is_pseudo_filesystem(path) {
            return false;
        }

        // Only the components below the root are judged: a root that is itself
        // hidden (`~/.local/share/notes` as a configured root) is a deliberate
        // choice by whoever configured it, not something to filter out.
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let mut components = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .peekable();

        while let Some(name) = components.next() {
            // The last component is the hit itself, and a hidden *file* is
            // reportable -- the walk reports `~/.bashrc` too, it just never
            // descends into a hidden directory. So only the ancestors are held
            // to the descent rule: that is precisely "could the walk have
            // arrived here".
            if components.peek().is_some() && !is_descendable_name(name) {
                return false;
            }
        }

        true
    }

    /// The deadline-bounded breadth-first walk (strategy (a)).
    ///
    /// Coverage is earned, not assumed: [`FileSearchCoverage::Complete`] means
    /// the queue drained with every directory read and every hit kept.
    /// A directory the process may not read, or a limit that cut the walk short,
    /// makes the answer [`FileSearchCoverage::Partial`] -- the results are real
    /// but there is provably more.
    fn walk(&self, needle: &str, limit: usize, started: Instant, deadline: Duration) -> FileSearchResults {
        let mut hits = Vec::new();
        let mut queue: VecDeque<PathBuf> = self
            .roots
            .iter()
            .filter(|root| !is_pseudo_filesystem(root))
            .cloned()
            .collect();
        let mut incomplete = self.roots.len() != queue.len();
        let mut truncated = false;
        let mut until_check = DEADLINE_CHECK_STRIDE;

        'walk: while let Some(directory) = queue.pop_front() {
            if started.elapsed() >= deadline {
                return FileSearchResults {
                    hits,
                    coverage: FileSearchCoverage::Deadline,
                };
            }

            let Ok(entries) = fs::read_dir(&directory) else {
                // Unreadable, gone, or not a directory at all. There is more
                // out there than this answer covers, and saying so is the
                // whole point of the coverage field.
                incomplete = true;
                continue;
            };

            for entry in entries {
                until_check -= 1;
                if until_check == 0 {
                    until_check = DEADLINE_CHECK_STRIDE;
                    if started.elapsed() >= deadline {
                        return FileSearchResults {
                            hits,
                            coverage: FileSearchCoverage::Deadline,
                        };
                    }
                }

                let Ok(entry) = entry else {
                    incomplete = true;
                    continue;
                };
                let Ok(file_type) = entry.file_type() else {
                    incomplete = true;
                    continue;
                };
                let name = entry.file_name();

                if name_matches(&name, needle) {
                    if hits.len() >= limit {
                        truncated = true;
                        break 'walk;
                    }
                    hits.push(walked_hit(&entry, &name, file_type));
                }

                // Directories only, and never through a symlink: following one
                // needs a visited set keyed by device and inode to stay
                // terminating, and `~/link-to-home` is a loop an ordinary user
                // can create by accident. The link itself is still reported
                // above, so it is findable -- it is just not a way in.
                if file_type.is_dir() {
                    let path = entry.path();
                    if is_descendable_name(&name) && !is_pseudo_filesystem(&path) {
                        queue.push_back(path);
                    }
                }
            }
        }

        let coverage = if truncated || incomplete {
            FileSearchCoverage::Partial
        } else {
            FileSearchCoverage::Complete
        };

        FileSearchResults { hits, coverage }
    }
}

impl Default for FilesystemSearch {
    fn default() -> Self {
        Self::for_session()
    }
}

impl FileSearchService for FilesystemSearch {
    /// The mechanism, not the platform: a user comparing two Linux machines
    /// needs to know that one answered from an index and the other from a walk,
    /// because that is what explains the difference in speed and freshness.
    fn source_name(&self) -> &'static str {
        if self.uses_index() {
            "plocate"
        } else {
            "filesystem-walk"
        }
    }

    fn search(&self, query: &FileSearchQuery) -> Result<FileSearchResults> {
        let started = Instant::now();
        let limit = query.limit.min(MAX_FILE_HITS);
        // The caller normalises, but the fold has to hold even if a caller
        // forgets: an unfolded needle would silently match nothing here.
        let needle = query.normalized.trim().to_lowercase();

        if self.roots.is_empty() {
            // Nothing configured to look at. Not an error -- an empty answer
            // from a service with no roots is exactly true -- but it covers
            // nothing, and must not claim to be complete.
            return Ok(FileSearchResults {
                hits: Vec::new(),
                coverage: FileSearchCoverage::Partial,
            });
        }
        if needle.is_empty() || limit == 0 {
            // A query with nothing to match, or a caller that asked for no
            // hits: there is no work to leave undone, so this is complete.
            return Ok(FileSearchResults {
                hits: Vec::new(),
                coverage: FileSearchCoverage::Complete,
            });
        }

        if let Some(binary) = &self.locate {
            if let Some(hits) = self.delegate_to_locate(binary, &needle, limit, started, query.deadline) {
                // Never `Complete`: the index is as old as the last `updatedb`,
                // so a file saved since then is missing from this answer and
                // the caller is entitled to know that.
                return Ok(FileSearchResults {
                    hits,
                    coverage: FileSearchCoverage::Partial,
                });
            }
        }

        Ok(self.walk(&needle, limit, started, query.deadline))
    }
}

/// The first executable named [`LOCATE_BINARY`] on `PATH`, if any.
///
/// `PATH` is scanned here rather than by spawning and letting the kernel look,
/// because [`FilesystemSearch::uses_index`] has to answer for capability
/// reporting before any search runs -- and reporting must not fork a process.
fn locate_binary() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;

    env::split_paths(&path)
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(LOCATE_BINARY))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        })
}

/// A hit built from a directory entry the walk already has in hand.
///
/// One `lstat` per *matching* entry and none for the rest: the timestamp is
/// worth a syscall for a row the user may see, and worth nothing for the
/// hundred thousand entries whose names did not match.
fn walked_hit(entry: &fs::DirEntry, name: &OsStr, file_type: FileType) -> FileHit {
    let path = entry.path();
    let kind = if file_type.is_dir() || (file_type.is_symlink() && points_at_directory(&path)) {
        FileKind::Directory
    } else {
        FileKind::File
    };

    FileHit {
        name: name.to_string_lossy().into_owned(),
        path: PlatformPath::new(path),
        kind,
        modified_unix_seconds: entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(unix_seconds),
    }
}

/// Whether a symlink resolves to a directory.
///
/// A symlink to a folder is a folder to the user -- it opens a file manager,
/// not an editor -- and the launcher ranks the two kinds differently, so this
/// one extra `stat` buys a correct row. A broken link resolves to nothing and
/// is reported as a file.
fn points_at_directory(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Case-insensitive substring test against a basename.
///
/// The ASCII path allocates nothing, which matters because it runs once per
/// directory entry on a keystroke. Only a non-ASCII name or needle pays for
/// the Unicode fold, and only that path is lossy -- a name that is not UTF-8
/// still matches by its bytes, and its `PlatformPath` is untouched either way
/// (spec 19.2).
fn name_matches(name: &OsStr, needle: &str) -> bool {
    let bytes = name.as_bytes();
    if needle.is_ascii() && bytes.is_ascii() {
        if needle.len() > bytes.len() {
            return false;
        }
        return bytes
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()));
    }

    name.to_string_lossy().to_lowercase().contains(needle)
}

/// Whether the walk may descend into a directory with this name.
///
/// Hidden directories are excluded because `~/.cache`, `~/.local` and
/// `~/.mozilla` hold more files than the rest of a home directory put together
/// and none of them is a document; excluding them is what makes a walk of
/// `$HOME` finishable inside a keystroke's budget at all.
fn is_descendable_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    if bytes.first() == Some(&b'.') {
        return false;
    }

    !EXCLUDED_DIRECTORY_NAMES
        .iter()
        .any(|excluded| name == OsStr::new(excluded))
}

/// Whether a path is inside a kernel interface rather than a filesystem.
fn is_pseudo_filesystem(path: &Path) -> bool {
    EXCLUDED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(Path::new(prefix)))
}

/// Seconds since the Unix epoch, negative before it.
///
/// Saturating rather than wrapping, and never a fabricated zero: a timestamp
/// the kernel reports as absurd must not rank as "modified in 1970" (see
/// [`FileHit::modified_unix_seconds`]).
fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs()).map_or(i64::MIN, |seconds| -seconds),
    }
}
