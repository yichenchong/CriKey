//! Searching the user's files and folders on macOS (spec 18.1, 18.2).
//!
//! # Two mechanisms, one service
//!
//! Spotlight is asked first. It is the index macOS already maintains, it is
//! what every native launcher on the platform is a front end for, and it
//! answers in the low milliseconds a keystroke can afford. When it declines --
//! and there are three separate ways it declines silently, below -- a bounded
//! breadth-first walk of the user's home directory answers instead. A thin
//! answer is worth more than a spinner: the module contract in
//! [`crikey_platform::file_search`] says partial truth beats silence.
//!
//! # Why Spotlight can never report [`FileSearchCoverage::Complete`]
//!
//! Three failure modes are indistinguishable from "there are no matching
//! files", and all three are ordinary on a real machine:
//!
//! * **TCC.** A scope the process has not been granted -- Desktop, Documents,
//!   Downloads, an external volume -- is omitted from the result set. No error
//!   is raised and no attribute records the omission.
//! * **Spotlight switched off.** `mdutil -i off` on a volume, or `-d` for the
//!   whole machine, leaves the query API in place and answering with nothing.
//!   Apple documents no loud failure for this.
//! * **An index still being built.** A freshly restored machine answers
//!   truthfully and narrowly for hours.
//!
//! Since none of these can be told apart from an honest miss, every Spotlight
//! answer is reported as [`FileSearchCoverage::Partial`], and the session
//! capability is [`CapabilityState::Partial`] whenever Spotlight is in play.
//! The walk, whose exclusions are this file's own and therefore knowable, is
//! allowed to say [`FileSearchCoverage::Complete`].
//!
//! # `kMDItemPath` is not a queryable attribute
//!
//! Apple documents `kMDItemPath` as retrievable only: it may be read back off
//! a result item but may not appear in a query expression or a sort
//! descriptor. A query that names it does not fail loudly -- `MDQueryCreate`
//! simply returns NULL. The predicate here therefore matches on
//! `kMDItemFSName` and `kMDItemDisplayName`, and the path is read from the
//! [`MDItem`] afterwards. That also happens to be the behaviour the contract
//! wants: names are scored, paths are opened.
//!
//! # Rejected: `searchfs(2)`
//!
//! `searchfs(2)` searches a whole volume's catalogue directly and beats a
//! `find` by roughly two orders of magnitude, which makes it a tempting third
//! option. It was rejected twice over. Apple's own man page carries a
//! compatibility note stating the call has been undocumented for over two
//! years and that behaviour varies per volume implementation, so no two file
//! systems answer it alike. Worse, it yields catalogue entries rather than
//! paths, and turning one into a path requires `fsgetpath`, which is private
//! SPI -- unusable in a distributable, unshippable through the App Store, and
//! liable to vanish. Recorded here so the next person does not rediscover it.

use crikey_core::{PlatformPath, Result};
use crikey_platform::{
    CapabilityState, FileHit, FileKind, FileSearchCoverage, FileSearchQuery, FileSearchResults,
    FileSearchService, MAX_FILE_HITS,
};
use std::collections::VecDeque;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// The share of the caller's deadline Spotlight may spend before the walk gets
/// what is left.
///
/// Spotlight answers in milliseconds when it answers at all, so the split
/// matters only in the failing case -- a stalled `mds`, a volume being
/// re-indexed. Two thirds keeps the common case unconstrained while leaving
/// the fallback enough time to reach the first two or three levels of the home
/// directory, which is where a launcher's useful hits live.
const SPOTLIGHT_DEADLINE_NUMERATOR: u32 = 2;
const SPOTLIGHT_DEADLINE_DENOMINATOR: u32 = 3;

/// Longest query text handed to Spotlight or matched by the walk.
///
/// A predicate is built by interpolating the user's text twice, and escaping
/// can double every character, so an unbounded query text is an unbounded
/// string built on a keystroke. No filename search is meaningfully more
/// selective past this length.
const MAX_NEEDLE_CHARS: usize = 128;

/// Directory entries the walk inspects between two clock reads.
///
/// [`Instant::now`] is a `mach_absolute_time` call: cheap, but not free, and
/// the walk performs one comparison per entry either way. Sampling the clock
/// every few hundred entries bounds the overshoot past the deadline to the
/// time it takes to `stat`-and-compare that many names, which is microseconds.
const DEADLINE_CHECK_INTERVAL: usize = 512;

/// Directory entries one walk may inspect regardless of the deadline.
///
/// The deadline is the real bound; this is the guard against a pathological
/// home directory -- a mounted network share, a checked-out monorepo -- making
/// one search allocate an unbounded queue of directories still to visit before
/// the clock is next read.
const MAX_WALKED_ENTRIES: usize = 200_000;

/// Directory names the walk refuses to descend into, by exact name.
///
/// `Library` is the user's half of the operating system: caches, mail stores,
/// container sandboxes, simulator images. It is the largest thing in a home
/// directory by a wide margin and almost nothing in it has a name a person
/// would type. `node_modules` is the same argument in miniature, and a single
/// checkout can contain hundreds of them.
const SKIPPED_DIRECTORY_NAMES: &[&[u8]] = &[b"Library", b"node_modules"];

/// Directory-name suffixes the walk treats as opaque leaves.
///
/// Each of these *is* a directory, but macOS presents it as one object: a user
/// looking for `Photos Library` wants the library, not the forty thousand
/// files inside it, and a user looking for an application wants the bundle,
/// not its `Contents/Resources`. Descending would multiply the entry count by
/// orders of magnitude in exchange for hits nobody asked for.
const OPAQUE_DIRECTORY_SUFFIXES: &[&[u8]] = &[
    b".app",
    b".bundle",
    b".framework",
    b".photoslibrary",
    b".musiclibrary",
    b".tvlibrary",
];

/// File search over Spotlight, falling back to a bounded walk of `$HOME`.
///
/// The roots are resolved on first use rather than in the constructor: reading
/// `HOME` is environment access, and [`MacOsBackend::new`] is documented to
/// touch nothing. `spotlight` is a construction-time choice rather than a
/// probe because there is no probe -- see the module note -- so the only
/// honest switch is the caller's.
///
/// [`MacOsBackend::new`]: crate::MacOsBackend::new
#[derive(Debug)]
pub struct MacFileSearch {
    /// Where the fallback walk begins. Empty means the walk finds nothing,
    /// which is the truthful answer for a process with no home directory.
    roots: OnceLock<Vec<PathBuf>>,
    /// Whether Spotlight is consulted before the walk.
    spotlight: bool,
}

impl MacFileSearch {
    /// The name reported when Spotlight answered, or could have.
    const SPOTLIGHT_SOURCE: &'static str = "spotlight";

    /// The name reported by a build that only walks.
    const WALK_SOURCE: &'static str = "home-directory walk";

    /// Spotlight first, then a walk of the user's home directory.
    pub fn new() -> Self {
        Self {
            roots: OnceLock::new(),
            spotlight: true,
        }
    }

    /// Walks exactly these roots and never consults Spotlight.
    ///
    /// This is how the walk is exercised on a machine whose Spotlight index
    /// would otherwise answer first and hide it, and how a session that knows
    /// Spotlight is off avoids paying for a query that cannot succeed.
    pub fn walking(roots: Vec<PathBuf>) -> Self {
        let cell = OnceLock::new();
        // The cell is fresh, so the only way `set` can fail is a bug here.
        let _ = cell.set(roots);
        Self {
            roots: cell,
            spotlight: false,
        }
    }

    /// What this session can honestly claim for
    /// [`Capability::FileSearch`](crikey_platform::Capability::FileSearch).
    ///
    /// `Partial` whenever Spotlight is consulted, because a TCC-withheld scope
    /// and a disabled index both answer with silence rather than an error, so
    /// this backend cannot know what it is not being shown. A walk-only build
    /// knows exactly what it excludes and can claim `Available`.
    pub fn capability_state(&self) -> CapabilityState {
        if self.spotlight {
            CapabilityState::Partial
        } else {
            CapabilityState::Available
        }
    }

    /// The directories the fallback walk starts from, resolved once.
    fn roots(&self) -> &[PathBuf] {
        self.roots.get_or_init(|| {
            match env::var_os("HOME").filter(|home| !home.is_empty()) {
                Some(home) => vec![PathBuf::from(home)],
                // A launcher started without `HOME` -- a `launchd` job, a
                // stripped test harness -- has no home to walk. Reporting no
                // hits is right; guessing at `/Users/<something>` is not.
                None => Vec::new(),
            }
        })
    }
}

impl Default for MacFileSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSearchService for MacFileSearch {
    fn source_name(&self) -> &'static str {
        if self.spotlight {
            Self::SPOTLIGHT_SOURCE
        } else {
            Self::WALK_SOURCE
        }
    }

    fn search(&self, query: &FileSearchQuery) -> Result<FileSearchResults> {
        let started = Instant::now();
        let limit = query.limit.min(MAX_FILE_HITS);
        let needle = query.normalized.trim();

        // An empty query names every file, which is not a search; a zero limit
        // asks for no rows. Both are answered without touching the disk, and
        // both are honestly complete: everything asked for was returned.
        if needle.is_empty() || needle.chars().count() > MAX_NEEDLE_CHARS || limit == 0 {
            return Ok(FileSearchResults {
                hits: Vec::new(),
                coverage: FileSearchCoverage::Complete,
            });
        }

        if self.spotlight {
            let budget = query.deadline * SPOTLIGHT_DEADLINE_NUMERATOR / SPOTLIGHT_DEADLINE_DENOMINATOR;
            // `None` is "Spotlight did not answer"; an empty answer is one of
            // the three silent declines. Neither can be distinguished from an
            // honest miss, so both fall through to the walk rather than being
            // reported as "no such file".
            if let Some(hits) = spotlight::search(needle, limit, budget) {
                if !hits.is_empty() {
                    return Ok(FileSearchResults {
                        hits,
                        coverage: FileSearchCoverage::Partial,
                    });
                }
            }
        }

        let (hits, coverage) = walk(self.roots(), needle, limit, started, query.deadline);
        Ok(FileSearchResults { hits, coverage })
    }
}

/// Breadth-first walk of `roots`, bounded by `deadline` measured from `started`.
///
/// Breadth first rather than depth first because depth is a proxy for
/// irrelevance: `~/Notes/todo.md` matters more than the eleventh copy of
/// `todo.md` inside a dependency tree, and a walk that runs out of time should
/// have spent it near the top. It also makes the deadline degrade gracefully
/// -- a truncated breadth-first walk is a shallow search, while a truncated
/// depth-first walk is one arbitrary branch.
fn walk(
    roots: &[PathBuf],
    needle: &str,
    limit: usize,
    started: Instant,
    deadline: Duration,
) -> (Vec<FileHit>, FileSearchCoverage) {
    let mut hits = Vec::new();
    let mut queue: VecDeque<PathBuf> = roots.iter().cloned().collect();
    let mut inspected = 0usize;

    while let Some(directory) = queue.pop_front() {
        if started.elapsed() >= deadline {
            return (hits, FileSearchCoverage::Deadline);
        }
        // An unreadable directory is not an error: a home directory contains
        // sockets, dead mount points and folders the user has locked, and a
        // search that gave up on the first one would be useless.
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };

            inspected += 1;
            if inspected >= MAX_WALKED_ENTRIES {
                return (hits, FileSearchCoverage::Partial);
            }
            if inspected % DEADLINE_CHECK_INTERVAL == 0 && started.elapsed() >= deadline {
                return (hits, FileSearchCoverage::Deadline);
            }

            let name = entry.file_name();
            // Dot files are the machine's business, not the user's. Skipping
            // them here rather than at match time also keeps the walk out of
            // `.git`, `.cache` and `.Trash`, which between them can hold more
            // entries than the visible home directory does.
            if name.as_bytes().first() == Some(&b'.') {
                continue;
            }

            let file_type = entry.file_type().ok();
            let is_directory = file_type.is_some_and(|kind| kind.is_dir());

            if name_matches(&name, needle) {
                if hits.len() >= limit {
                    return (hits, FileSearchCoverage::Partial);
                }
                let path = entry.path();
                let kind = if is_directory {
                    FileKind::Directory
                } else if file_type.is_some_and(|kind| kind.is_symlink())
                    && fs::metadata(&path).is_ok_and(|target| target.is_dir())
                {
                    // The walk never follows a symlink -- that is how a home
                    // directory turns into a cycle -- but a link to a folder
                    // still opens like a folder, and one `stat` per *hit* is
                    // affordable where one per entry would not be.
                    FileKind::Directory
                } else {
                    FileKind::File
                };
                hits.push(FileHit {
                    name: name.to_string_lossy().into_owned(),
                    path: PlatformPath::new(path),
                    kind,
                    modified_unix_seconds: modified_seconds(&entry),
                });
            }

            if is_directory && !is_opaque(&name) {
                queue.push_back(entry.path());
            }
        }
    }

    (hits, FileSearchCoverage::Complete)
}

/// Whether `name` contains `needle`, which the caller has already lowercased.
///
/// The ASCII path exists because the alternative allocates. `to_lowercase`
/// builds a `String` per directory entry, and this runs over six figures of
/// entries inside one keystroke's deadline; almost every filename on a real
/// machine is ASCII, and `eq_ignore_ascii_case` needs no allocation at all.
/// The general path is kept for the names that are not.
fn name_matches(name: &OsStr, needle: &str) -> bool {
    let text = name.to_string_lossy();
    if text.is_ascii() && needle.is_ascii() {
        let (haystack, needle) = (text.as_bytes(), needle.as_bytes());
        if needle.len() > haystack.len() {
            return false;
        }
        return haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle));
    }
    text.to_lowercase().contains(needle)
}

/// Whether the walk treats a directory of this name as a leaf.
fn is_opaque(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    if SKIPPED_DIRECTORY_NAMES.contains(&bytes) {
        return true;
    }
    OPAQUE_DIRECTORY_SUFFIXES.iter().any(|suffix| {
        bytes.len() > suffix.len() && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    })
}

/// A directory entry's modification time, or `None` when it cannot be had.
///
/// `None` rather than zero, per the [`FileHit`] contract: a file whose time is
/// unreadable must not rank as though it were last touched in 1970.
fn modified_seconds(entry: &fs::DirEntry) -> Option<i64> {
    let modified = entry.metadata().ok()?.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).ok(),
        // A timestamp before 1970 is rare but legal, and negating it is the
        // whole reason the field is signed.
        Err(before) => i64::try_from(before.duration().as_secs()).ok().map(|s| -s),
    }
}

/// The Spotlight half: `MDQuery` over the metadata index.
mod spotlight {
    // Every call in this module is a C entry point taking raw CoreFoundation
    // pointers; there is no safe binding to Spotlight, and wrapping each call
    // individually would put an `unsafe` block on every line of the gather
    // loop without documenting anything the module note does not already say.
    // Matches the precedent in `crikey-sandbox/src/landlock.rs`.
    #![allow(unsafe_code)]

    use super::{FileHit, FileKind, PlatformPath, MAX_NEEDLE_CHARS};
    use objc2_core_foundation::{
        kCFAbsoluteTimeIntervalSince1970, CFArray, CFDate, CFIndex, CFOptionFlags, CFRetained, CFString,
    };
    use objc2_core_services::{
        kMDItemContentModificationDate, kMDItemContentType, kMDItemFSName, kMDItemPath, kMDQueryScopeHome,
        MDItem, MDQuery, MDQueryOptionFlags,
    };
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// Whether a Spotlight query is running right now.
    ///
    /// One at a time, process-wide. An abandoned query is still *running* —
    /// `MDQueryExecute` has no cancellation, so a thread that overran its
    /// budget keeps sitting in `mds` until `mds` answers. Without this permit
    /// a user holding down a key while `mds` is wedged spawns one more
    /// abandoned thread per keystroke, each pinned in a C call nothing can
    /// interrupt, and the deadline that was supposed to protect the launcher
    /// instead sets the rate at which it leaks threads.
    static QUERY_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

    /// Consecutive overruns before Spotlight is left alone.
    ///
    /// Two rather than one: a single slow query is ordinary on a machine that
    /// has just woken or is mid-reindex, and giving up on the whole index for
    /// one of those would be an overreaction.
    const TIMEOUTS_BEFORE_GIVING_UP: u32 = 2;

    /// Overruns seen in a row. Reset by any answer, however empty.
    static CONSECUTIVE_TIMEOUTS: AtomicU32 = AtomicU32::new(0);

    /// The uniform type identifier every plain folder carries.
    ///
    /// Deliberately the only directory-ish UTI treated as a directory: an
    /// application bundle, a Photos library and an `.rtfd` are all directories
    /// on disk, and all three are things a user opens rather than enters.
    const FOLDER_TYPE: &str = "public.folder";

    /// Files and folders Spotlight knows whose name contains `needle`.
    ///
    /// `None` means Spotlight did not answer within `budget`, could not be
    /// asked, or refused the query -- never "there are no such files". The
    /// caller cannot tell the three apart and must not present any of them as
    /// an empty result set.
    ///
    /// # Why a thread, and why only ever one
    ///
    /// `MDQueryExecute` with `kMDQuerySynchronous` blocks until the gather
    /// phase finishes, and nothing in the API caps how long that is: a machine
    /// mid-reindex, or one with a stalled `mds`, can sit there for seconds.
    /// The deadline in [`FileSearchQuery`] is a promise the caller relies on
    /// per keystroke, so the blocking call happens on a thread of its own and
    /// the result arrives over a channel. A query that overruns is abandoned,
    /// not waited for: the thread finishes into a dropped receiver and exits.
    ///
    /// Abandoning is only safe because it is bounded. The query has no
    /// cancellation, so an abandoned thread stays alive until `mds` answers
    /// it; `QUERY_IN_FLIGHT` therefore admits one at a time, and a caller that
    /// arrives while one is outstanding gets `None` and walks instead of
    /// queueing behind a call that may never return. After
    /// `TIMEOUTS_BEFORE_GIVING_UP` consecutive overruns Spotlight is dropped
    /// for the rest of the session: an index that cannot answer twice running
    /// will not answer the third time either, and every attempt costs the user
    /// two thirds of a keystroke's budget before the walk even starts.
    ///
    /// [`FileSearchQuery`]: crikey_platform::FileSearchQuery
    pub(super) fn search(needle: &str, limit: usize, budget: Duration) -> Option<Vec<FileHit>> {
        if CONSECUTIVE_TIMEOUTS.load(Ordering::Relaxed) >= TIMEOUTS_BEFORE_GIVING_UP {
            return None;
        }
        let predicate = predicate(needle)?;
        // Acquire the single permit. Failure means another keystroke's query
        // is still inside `mds`; the walk is the better answer.
        if QUERY_IN_FLIGHT
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        let (sender, receiver) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("crikey-spotlight".to_owned())
            .spawn(move || {
                // SAFETY: `gather` owns every CoreFoundation object it makes
                // and hands back only owned Rust values, so nothing borrowed
                // from this thread outlives it.
                let found = unsafe { gather(&predicate, limit) };
                // The receiver is gone when the deadline expired first. That
                // is the designed outcome, not an error.
                let _ = sender.send(found);
                // Released here rather than by the caller: the permit tracks
                // the QUERY, which outlives an abandoned wait.
                QUERY_IN_FLIGHT.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            // A process that cannot spawn a thread cannot run a query either,
            // but it must not keep the permit it never used.
            QUERY_IN_FLIGHT.store(false, Ordering::Release);
            return None;
        }
        match receiver.recv_timeout(budget) {
            Ok(found) => {
                CONSECUTIVE_TIMEOUTS.store(0, Ordering::Relaxed);
                found
            }
            Err(_) => {
                CONSECUTIVE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Runs one synchronous `MDQuery` and reads the results off it.
    ///
    /// # Safety
    ///
    /// Must be called on a thread that may block; `MDQueryExecute` runs the
    /// current thread's run loop in a private mode until the gather phase
    /// completes.
    unsafe fn gather(predicate: &str, limit: usize) -> Option<Vec<FileHit>> {
        let predicate = CFString::from_str(predicate);
        // NULL here is the documented answer to a malformed query, and the
        // only signal `MDQueryCreate` gives. Treat it as "Spotlight is not
        // answering", never as "no matches".
        let query = unsafe { MDQuery::new(None, Some(&predicate), None, None) }?;

        // The home directory rather than the whole computer: a launcher's file
        // results are the user's own files, and scoping the query is also the
        // cheapest way to keep `/System` and every installed application's
        // resources out of the answer.
        let scope = unsafe { kMDQueryScopeHome }?;
        let scopes = CFArray::from_objects(&[scope]);
        // `set_search_scope` takes the untyped `CFArray`; the element type is
        // erased here rather than at construction so the array is still built
        // through the checked `from_objects`.
        let scopes: &CFArray = (*scopes).as_ref();
        unsafe { query.set_search_scope(Some(scopes), 0) };
        unsafe { query.set_max_count(CFIndex::try_from(limit).ok()?) };

        let flags = MDQueryOptionFlags::Synchronous.0 as CFOptionFlags;
        // `false` means the query never started. Same reading as NULL above.
        if !unsafe { query.execute(flags) } {
            return None;
        }

        let count = unsafe { query.result_count() };
        let mut hits = Vec::with_capacity(count.max(0) as usize);
        for index in 0..count {
            let result = unsafe { query.result_at_index(index) };
            let Some(item) = (unsafe { result.cast::<MDItem>().as_ref() }) else {
                continue;
            };
            if let Some(hit) = unsafe { read_hit(item) } {
                hits.push(hit);
            }
        }
        Some(hits)
    }

    /// One result item as a [`FileHit`], or `None` if it lacks a usable path.
    ///
    /// # Safety
    ///
    /// `item` must be a live `MDItemRef` owned by a query that outlives the
    /// call.
    unsafe fn read_hit(item: &MDItem) -> Option<FileHit> {
        // `kMDItemPath` is read here rather than queried; see the module note.
        let path = unsafe { string_attribute(item, kMDItemPath) }?;
        if path.is_empty() {
            return None;
        }
        // The filesystem name, not the display name: the display name is
        // localised and extension-hidden, so scoring against it would match
        // text that is nowhere in the filename the user typed. The query
        // matches both, deliberately; only the query does.
        let name = unsafe { string_attribute(item, kMDItemFSName) }.unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .filter(|last| !last.is_empty())
                .unwrap_or(&path)
                .to_owned()
        });

        let kind = match unsafe { string_attribute(item, kMDItemContentType) } {
            Some(uti) if uti == FOLDER_TYPE => FileKind::Directory,
            _ => FileKind::File,
        };

        let modified = unsafe { item.attribute(kMDItemContentModificationDate) }
            .and_then(|value| value.downcast::<CFDate>().ok())
            .map(|date| {
                // `CFAbsoluteTime` counts from 2001-01-01; the constant is
                // Core Foundation's own epoch offset rather than a literal so
                // the conversion cannot drift from the framework's.
                let since_epoch = date.absolute_time() + unsafe { kCFAbsoluteTimeIntervalSince1970 };
                since_epoch as i64
            });

        Some(FileHit {
            name,
            path: PlatformPath::new(path),
            kind,
            modified_unix_seconds: modified,
        })
    }

    /// One string-valued metadata attribute, or `None` when it is absent or of
    /// another type.
    ///
    /// # Safety
    ///
    /// `item` must be a live `MDItemRef`.
    unsafe fn string_attribute(item: &MDItem, name: Option<&CFString>) -> Option<String> {
        let value = unsafe { item.attribute(name) }?;
        let text: CFRetained<CFString> = value.downcast::<CFString>().ok()?;
        Some(text.to_string())
    }

    /// The Spotlight query expression matching `needle` against both names.
    ///
    /// `cd` on each comparison is Spotlight's case- and diacritic-insensitive
    /// modifier, which is what makes `cafe` find `Café` -- the launcher's
    /// caller has already lowercased the text but cannot fold accents.
    ///
    /// `None` when nothing survives escaping, which would otherwise produce
    /// `"**"`: a predicate matching every indexed file on the machine.
    pub(super) fn predicate(needle: &str) -> Option<String> {
        let escaped = escape(needle)?;
        Some(format!(
            "(kMDItemFSName == \"*{escaped}*\"cd) || (kMDItemDisplayName == \"*{escaped}*\"cd)"
        ))
    }

    /// User text as a literal inside a double-quoted Spotlight value.
    ///
    /// `*` and `?` are wildcards inside the quotes, `"` ends the value and `\`
    /// starts an escape, so all four are backslash-escaped: a user typing `*`
    /// means the character, not "everything". Control characters are dropped
    /// rather than escaped -- Spotlight's grammar has no spelling for them,
    /// and no filename a person types contains one.
    fn escape(needle: &str) -> Option<String> {
        let mut escaped = String::with_capacity(needle.len());
        for character in needle.chars().take(MAX_NEEDLE_CHARS) {
            if character.is_control() {
                continue;
            }
            if matches!(character, '"' | '\\' | '*' | '?') {
                escaped.push('\\');
            }
            escaped.push(character);
        }
        (!escaped.is_empty()).then_some(escaped)
    }
}

#[cfg(test)]
mod tests {
    //! Contract for the fallback walk (spec 18.1).
    //!
    //! Spotlight cannot be pinned by a test: its answer depends on the host's
    //! index, its TCC grants and whether `mdutil` has been run, and a test
    //! that asserted on it would assert on the machine rather than on this
    //! code. What is testable is everything this file decides for itself --
    //! the walk's bounds, its exclusions, its coverage reporting -- plus the
    //! pure predicate construction, which is where a Spotlight bug would
    //! actually originate.
    //!
    //! Every case builds its own tree in a unique temp directory, so nothing
    //! here depends on the contents of the host's home directory.

    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique scratch directory that deletes itself when the test ends.
    ///
    /// Uniqueness comes from the process id plus a monotonic counter, never
    /// from a clock, so parallel test threads and repeated runs cannot
    /// collide. Mirrors the scanner tests in `lib.rs`.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!("crikey-macos-files-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("the scratch directory must be creatable");
            Self { path }
        }

        /// Creates `relative`'s parent directories and writes an empty file.
        fn file(&self, relative: &str) -> PathBuf {
            let path = self.path.join(relative);
            fs::create_dir_all(path.parent().expect("a file has a parent"))
                .expect("the tree must be creatable");
            fs::write(&path, b"").expect("the file must be writable");
            path
        }

        fn directory(&self, relative: &str) -> PathBuf {
            let path = self.path.join(relative);
            fs::create_dir_all(&path).expect("the tree must be creatable");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// A generous deadline: these trees hold single-digit numbers of files, so
    /// a case that hits this has hung, not merely run slowly.
    const GENEROUS: Duration = Duration::from_secs(30);

    fn search(service: &MacFileSearch, text: &str, limit: usize) -> FileSearchResults {
        service
            .search(&FileSearchQuery {
                normalized: text.to_owned(),
                limit,
                deadline: GENEROUS,
            })
            .expect("a walk of a readable directory never fails")
    }

    fn names(results: &FileSearchResults) -> Vec<String> {
        let mut found: Vec<String> = results.hits.iter().map(|hit| hit.name.clone()).collect();
        found.sort();
        found
    }

    /// The walk reaches below the roots and matches on any part of the name.
    #[test]
    fn a_nested_file_is_found_by_a_substring_of_its_name() {
        let scratch = Scratch::new();
        scratch.file("notes/2024/quarterly-report.md");
        scratch.file("notes/unrelated.txt");

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        let results = search(&service, "quarter", 16);

        assert_eq!(names(&results), vec!["quarterly-report.md".to_owned()]);
        assert_eq!(results.coverage, FileSearchCoverage::Complete);
    }

    /// Matching ignores case in both directions, which is what the ASCII fast
    /// path exists to do without allocating.
    #[test]
    fn matching_ignores_case() {
        let scratch = Scratch::new();
        scratch.file("Invoices/ACME-Invoice.pdf");

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        assert_eq!(
            names(&search(&service, "acme", 16)),
            vec!["ACME-Invoice.pdf".to_owned()]
        );
    }

    /// A directory is reported as one, because the launcher ranks it above a
    /// file of the same name.
    #[test]
    fn a_matching_directory_is_reported_as_a_directory() {
        let scratch = Scratch::new();
        scratch.directory("projects");
        scratch.file("projects.txt");

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        let results = search(&service, "projects", 16);

        let mut kinds: Vec<(String, FileKind)> = results
            .hits
            .iter()
            .map(|hit| (hit.name.clone(), hit.kind))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                ("projects".to_owned(), FileKind::Directory),
                ("projects.txt".to_owned(), FileKind::File),
            ]
        );
    }

    /// `Library` and dot directories are the two exclusions that make the walk
    /// affordable; a regression in either would flood every result list.
    #[test]
    fn the_excluded_directories_are_not_descended_into() {
        let scratch = Scratch::new();
        scratch.file("Library/Caches/target.txt");
        scratch.file("node_modules/left-pad/target.txt");
        scratch.file(".git/objects/target.txt");
        scratch.file("Documents/target.txt");

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        let results = search(&service, "target", 16);

        assert_eq!(names(&results), vec!["target.txt".to_owned()]);
        assert_eq!(
            results.hits[0].path.as_path(),
            scratch.path.join("Documents/target.txt"),
            "the only reachable copy is the one outside every exclusion"
        );
    }

    /// A bundle is one object. Its name still matches; its insides do not.
    #[test]
    fn a_bundle_matches_by_name_without_being_entered() {
        let scratch = Scratch::new();
        scratch.file("Photos Library.photoslibrary/originals/library-photo.jpg");

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        let results = search(&service, "library", 16);

        assert_eq!(names(&results), vec!["Photos Library.photoslibrary".to_owned()]);
    }

    /// The limit is a hard stop and it is reported, so the caller can say the
    /// list is a subset rather than the whole truth.
    #[test]
    fn reaching_the_limit_truncates_and_reports_partial() {
        let scratch = Scratch::new();
        for index in 0..8 {
            scratch.file(&format!("reports/report-{index}.txt"));
        }

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        let results = search(&service, "report", 3);

        assert_eq!(results.hits.len(), 3);
        assert_eq!(results.coverage, FileSearchCoverage::Partial);
    }

    /// An expired deadline yields what was found, not an error. This is the
    /// promise the module note calls a promise rather than a hint.
    #[test]
    fn an_expired_deadline_returns_results_rather_than_failing() {
        let scratch = Scratch::new();
        scratch.file("deep/deeper/deepest/target.txt");

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        let results = service
            .search(&FileSearchQuery {
                normalized: "target".to_owned(),
                limit: 16,
                deadline: Duration::ZERO,
            })
            .expect("running out of time is not a failure");

        assert_eq!(results.coverage, FileSearchCoverage::Deadline);
        assert!(results.hits.is_empty(), "no time was available to find anything");
    }

    /// An empty query is not a request for every file on the machine.
    #[test]
    fn an_empty_query_searches_nothing() {
        let scratch = Scratch::new();
        scratch.file("anything.txt");

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        let results = search(&service, "   ", 16);

        assert!(results.hits.is_empty());
        assert_eq!(results.coverage, FileSearchCoverage::Complete);
    }

    /// A limit above the ceiling is clamped rather than honoured, so no caller
    /// can make one search allocate without bound.
    #[test]
    fn the_hit_ceiling_bounds_an_oversized_limit() {
        let scratch = Scratch::new();
        scratch.file("one-file.txt");

        let service = MacFileSearch::walking(vec![scratch.path.clone()]);
        let results = search(&service, "file", MAX_FILE_HITS * 4);

        assert_eq!(results.hits.len(), 1);
    }

    /// A walk-only service claims exactly what it can deliver, and says which
    /// mechanism answered.
    #[test]
    fn a_walk_only_service_is_available_and_names_itself() {
        let service = MacFileSearch::walking(Vec::new());
        assert_eq!(service.capability_state(), CapabilityState::Available);
        assert_eq!(service.source_name(), MacFileSearch::WALK_SOURCE);
    }

    /// Spotlight cannot prove it saw everything, so the session never claims
    /// it did.
    #[test]
    fn the_spotlight_service_never_claims_full_coverage() {
        let service = MacFileSearch::new();
        assert_eq!(service.capability_state(), CapabilityState::Partial);
        assert_eq!(service.source_name(), MacFileSearch::SPOTLIGHT_SOURCE);
    }

    /// Wildcards in the user's text are characters, not grammar. Without the
    /// escape a user typing `*` would ask Spotlight for every indexed file.
    #[test]
    fn the_predicate_escapes_spotlight_wildcards_and_quotes() {
        let predicate = spotlight::predicate("re*port\"s?").expect("the text survives escaping");
        assert!(
            predicate.contains("*re\\*port\\\"s\\?*"),
            "every wildcard and quote must be literal, got {predicate}"
        );
        assert!(
            predicate.contains("kMDItemFSName") && predicate.contains("kMDItemDisplayName"),
            "both name attributes are queried, got {predicate}"
        );
        assert!(
            !predicate.contains("kMDItemPath"),
            "kMDItemPath is retrievable only and makes MDQueryCreate return NULL"
        );
    }

    /// Text that is nothing but control characters would escape to `"**"`,
    /// which matches the entire index. It must produce no query at all.
    #[test]
    fn text_that_escapes_to_nothing_produces_no_predicate() {
        assert!(spotlight::predicate("\u{0}\u{7}").is_none());
    }
}
