//! Searching this user's files and folders by name (spec 18.1, 18.2).
//!
//! # Two sources, one answer
//!
//! Windows already maintains an index of the user's content: the `SystemIndex`
//! catalog the Search service builds and Explorer queries. Reaching it is the
//! whole point of this module, because it is the only mechanism on the platform
//! that can answer a name query over a large tree inside a keystroke's budget.
//! [`win32::search`] asks it through the interfaces Explorer itself uses --
//! `CSearchManager` -> `GetCatalog("SystemIndex")` -> `GetQueryHelper` -> an OLE
//! DB `SELECT` over the connection string the helper hands out.
//!
//! The catalog is not the filesystem, though, and on a stock machine it is not
//! close to it. Windows 10 1903 introduced two indexing modes: *Classic*, the
//! clean-install default, indexes Documents, Pictures, Music and the Desktop;
//! *Enhanced* ("Find My Files") indexes the whole PC and is off by default. So
//! Downloads -- where a launcher is asked to look constantly -- is outside the
//! default catalog, and a catalog answer alone would teach the user the feature
//! is broken. [`WindowsFileSearch::walk`] therefore walks the profile's own
//! folders with whatever budget the catalog left, and the two answers are
//! merged. Neither source is ever the whole filesystem, which is why the
//! coverage this module reports is [`FileSearchCoverage::Partial`] on success
//! and never `Complete`.
//!
//! # Why the MFT is not one of the sources
//!
//! The fast whole-volume answer on NTFS is to enumerate the master file table
//! and tail the change journal, which is how Everything indexes a drive in
//! seconds. Both are `DeviceIoControl` operations -- `FSCTL_ENUM_USN_DATA`,
//! `FSCTL_READ_USN_JOURNAL` -- and Microsoft states that change journal
//! operations require system administrator privileges
//! (<https://learn.microsoft.com/en-us/windows/win32/fileio/using-the-change-journal-identifier>).
//! That is precisely why Everything installs an elevated service. CriKey is a
//! launcher: requiring elevation to type a filename is not a trade this
//! codebase makes, so the MFT route is deliberately not implemented, here or
//! behind a feature flag.
//!
//! # What cancellation stops
//!
//! The two sources honour a cancelled [`FileSearchQuery::cancel`] to different
//! degrees, and the difference is real rather than an implementation detail.
//! [`WindowsFileSearch::walk`] owns its loop and stops within one directory read
//! or 128 entries. The catalog is reached through synchronous OLE DB calls, so
//! [`win32`] can decline to start, stop collecting batches, and stop asking for
//! more of them, but it cannot take back a `GetNextRows` already inside the
//! Search service; its granularity is one batch. Either way the hits already
//! found come back as [`FileSearchCoverage::Cancelled`] rather than being
//! discarded, because the keystroke that superseded this search usually shares a
//! prefix with it.
//!
//! # What runs off Windows
//!
//! Everything except [`win32`]: the SQL builder, the escaping rules, the
//! `FILETIME` arithmetic and the walk are ordinary Rust, and the test suite
//! exercises all of them on every host. A query string that quotes wrongly and
//! a timestamp that lands in 1601 are exactly the failures that would be silent
//! on target, so they are not allowed to hide behind it.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "windows")]
use crikey_core::CoreError;
use crikey_core::{PlatformPath, Result};
use crikey_platform::{
    FileHit, FileKind, FileSearchCoverage, FileSearchQuery, FileSearchResults, FileSearchService,
    MAX_FILE_HITS,
};

#[cfg(target_os = "windows")]
mod win32;

/// The columns every `SystemIndex` query in this crate selects, in the order
/// [`win32`] binds them.
///
/// One column per field of [`FileHit`] plus the two the kind is derived from.
/// `System.FileAttributes` is the primary source for file-versus-folder because
/// it is a bit mask and therefore locale-independent, while `System.ItemType`
/// carries the literal `Directory` for a folder and is the documented fallback.
/// `System.Search.Contents` is deliberately absent: it is restriction-only, and
/// the shared contract forbids content search regardless.
pub const SELECT_COLUMNS: &str =
    "System.ItemPathDisplay, System.FileName, System.ItemType, System.FileAttributes, System.DateModified";

/// The profile subdirectories the fallback walk visits, in the order it visits
/// them.
///
/// These names are the ones on disk, not the ones Explorer shows: Windows
/// localises the display name of a known folder through its `desktop.ini`, and
/// the directory itself keeps the English name on every locale. Downloads and
/// Videos matter most here -- they are the two a Classic-mode catalog does not
/// contain -- and `AppData` is deliberately absent, because a launcher offering
/// a user twenty thousand cache files is worse than one offering none.
pub const WALK_SUBDIRECTORIES: &[&str] =
    &["Desktop", "Documents", "Downloads", "Pictures", "Music", "Videos"];

/// Nothing has answered yet, so no mechanism can be named.
const ANSWERED_NOTHING: u8 = 0;
/// The last answer came from the `SystemIndex` catalog alone.
const ANSWERED_INDEX: u8 = 1;
/// The last answer came from the directory walk alone.
const ANSWERED_WALK: u8 = 2;
/// The last answer was the catalog's and the walk's, merged.
const ANSWERED_BOTH: u8 = 3;

/// File search over the Windows Search catalog and this user's own folders.
#[derive(Debug)]
pub struct WindowsFileSearch {
    /// The directories [`Self::walk`] visits, in order.
    roots: Vec<PathBuf>,
    /// Whether the `SystemIndex` catalog may be consulted at all. False for a
    /// searcher built by [`Self::with_roots`], which is how the walk is pinned
    /// by a test on a host that has no catalog to consult.
    index: bool,
    /// Which mechanism answered the most recent search, for
    /// [`FileSearchService::source_name`]. One `Relaxed` store per search: the
    /// value is a diagnostic, so a reader racing a search and seeing the
    /// previous answer's source is not a defect worth a fence.
    answered: AtomicU8,
}

impl WindowsFileSearch {
    /// Reported by [`FileSearchService::source_name`] when the catalog answered
    /// on its own.
    pub const INDEX_SOURCE: &'static str = "windows-search-index";
    /// Reported when only the directory walk answered.
    pub const WALK_SOURCE: &'static str = "windows-directory-walk";
    /// Reported when both did, which is the ordinary case on a machine whose
    /// indexing mode is Classic.
    pub const MERGED_SOURCE: &'static str = "windows-search-index+walk";
    /// Reported before the first search, when no mechanism has been tried.
    pub const UNTRIED_SOURCE: &'static str = "windows-file-search";

    /// How deep below a root the fallback walk descends.
    ///
    /// A bound on the pathological case -- a junction pointing at an ancestor,
    /// a build tree nested twenty levels down -- not a policy. Reparse points
    /// are never descended into, so the cap is a second line of defence.
    pub const MAX_DEPTH: usize = 12;

    /// How many directory entries the walk inspects between two checks of the
    /// clock and of the caller's cancellation token.
    ///
    /// `Instant::now` is cheap but not free, and a walk that checks it per entry
    /// spends measurable time asking what time it is. Every 128 entries bounds
    /// the overrun to one directory read's worth of work. The cancellation
    /// token is polled at the same points, as the shared contract requires, and
    /// costs one atomic load per stride.
    pub const DEADLINE_CHECK_STRIDE: usize = 128;

    /// Searches the `SystemIndex` catalog and this user's profile folders.
    ///
    /// Construction reads the environment once for `%USERPROFILE%` and touches
    /// neither the filesystem nor COM: the catalog is reached on the first
    /// search, and a searcher that is built and never used costs one allocation.
    /// Off Windows the root list is empty and there is no catalog, so
    /// [`FileSearchService::search`] refuses rather than answering emptily.
    pub fn new() -> Self {
        Self::from_parts(walk_roots(), true)
    }

    /// Searches exactly these directories, in this order, and never the
    /// catalog.
    ///
    /// This is the host-independent constructor: a searcher told where to look
    /// answers from there on any operating system, which is what lets the walk's
    /// rules -- matching, ordering, the deadline, the limit -- be pinned by
    /// tests that run on Linux and macOS as well as on Windows.
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self::from_parts(roots, false)
    }

    fn from_parts(roots: Vec<PathBuf>, index: bool) -> Self {
        Self {
            roots,
            index,
            answered: AtomicU8::new(ANSWERED_NOTHING),
        }
    }

    /// The directories the fallback walk visits, in order.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether a search will consult the Windows Search catalog.
    ///
    /// False off Windows however this searcher was built, because there is no
    /// catalog to consult there -- the flag records permission, this records
    /// possibility.
    pub fn uses_index(&self) -> bool {
        self.index && cfg!(target_os = "windows")
    }

    /// What the `SystemIndex` catalog has to say, or the reason it says nothing.
    ///
    /// `Err` means the catalog is not a source for this search -- it was not
    /// permitted, the query was not worth asking, the service refused, or its
    /// status says it cannot answer right now -- and the walk must carry the
    /// answer alone. The reason travels with the refusal so that a session with
    /// no folders to walk either can tell the user what actually went wrong
    /// instead of reporting a bare absence.
    #[cfg(target_os = "windows")]
    fn indexed(&self, query: &FileSearchQuery, started: Instant, limit: usize) -> Result<FileSearchResults> {
        if !self.index {
            return Err(CoreError::Invalid(
                "this searcher was built for a fixed set of directories and does not consult the \
                 Windows Search index"
                    .to_owned(),
            ));
        }
        let sql = system_index_sql(query)
            .ok_or_else(|| CoreError::Invalid("an empty query has nothing to ask the index".to_owned()))?;
        win32::search(&sql, started, query.deadline, &query.cancel, limit)
    }

    /// There is no catalog on a build that is not Windows, whatever the searcher
    /// was permitted to consult, so the refusal is the crate's ordinary
    /// off-target one.
    #[cfg(not(target_os = "windows"))]
    fn indexed(
        &self,
        _query: &FileSearchQuery,
        _started: Instant,
        _limit: usize,
    ) -> Result<FileSearchResults> {
        Err(crate::off_target("search files"))
    }

    /// Files and folders under [`Self::roots`] whose name contains the query.
    ///
    /// `elapsed` is measured from `started`, so a caller that has already spent
    /// part of the deadline elsewhere -- on the catalog, for instance -- gets
    /// the walk it can still afford rather than a second full budget.
    ///
    /// Breadth first, because a match nearer a root is nearly always the one
    /// wanted and a deep tree must not starve its siblings. Reparse points are
    /// reported when they match but never descended into, so a junction cycle
    /// cannot make the walk run forever. Coverage is never `Complete`: these
    /// roots are a handful of directories, not the filesystem.
    ///
    /// This is the one part of the backend that stops the instant it is asked
    /// to: the loop is ours, so a cancellation is noticed within one directory
    /// read or 128 entries and the hits found so far come back as
    /// [`FileSearchCoverage::Cancelled`].
    pub fn walk(&self, query: &FileSearchQuery, started: Instant) -> FileSearchResults {
        let limit = query.limit.min(MAX_FILE_HITS);
        let mut hits = Vec::new();
        if limit == 0 || query.normalized.is_empty() {
            return FileSearchResults {
                hits,
                coverage: FileSearchCoverage::Partial,
            };
        }

        let mut coverage = FileSearchCoverage::Partial;
        let mut pending: VecDeque<(PathBuf, usize)> =
            self.roots.iter().map(|root| (root.clone(), 0)).collect();
        let mut inspected = 0usize;

        'walk: while let Some((directory, depth)) = pending.pop_front() {
            // Cancellation before the deadline because it is the stronger fact:
            // a caller who has given up gets told that, not that the clock ran
            // out. This loop is ours, so the stop is one directory read away.
            if query.cancel.is_cancelled() {
                coverage = FileSearchCoverage::Cancelled;
                break;
            }
            if started.elapsed() >= query.deadline {
                coverage = FileSearchCoverage::Deadline;
                break;
            }
            // A missing or unreadable directory is ordinary -- a profile without
            // a Videos folder, a folder the process may not read -- and must not
            // delete the hits every other root contributed.
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };

            for entry in entries.flatten() {
                inspected += 1;
                if inspected % Self::DEADLINE_CHECK_STRIDE == 0 {
                    if query.cancel.is_cancelled() {
                        coverage = FileSearchCoverage::Cancelled;
                        break 'walk;
                    }
                    if started.elapsed() >= query.deadline {
                        coverage = FileSearchCoverage::Deadline;
                        break 'walk;
                    }
                }

                // `file_type` reads what the directory entry already carries and
                // does not follow the link, so a junction is classified as a
                // link rather than as the directory it points at.
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() && depth < Self::MAX_DEPTH {
                    pending.push_back((entry.path(), depth + 1));
                }

                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.to_lowercase().contains(&query.normalized) {
                    continue;
                }

                hits.push(FileHit {
                    name: name.into_owned(),
                    path: PlatformPath::new(entry.path().into_os_string()),
                    kind: if file_type.is_dir() {
                        FileKind::Directory
                    } else {
                        FileKind::File
                    },
                    // Only matched entries are stat'ed: the timestamp is worth a
                    // syscall for a row the user may see and worth none for the
                    // thousands they will not.
                    modified_unix_seconds: entry
                        .metadata()
                        .ok()
                        .and_then(|metadata| metadata.modified().ok())
                        .and_then(unix_seconds),
                });
                if hits.len() >= limit {
                    break 'walk;
                }
            }
        }

        FileSearchResults { hits, coverage }
    }
}

impl Default for WindowsFileSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSearchService for WindowsFileSearch {
    /// Which mechanism answered the last search: the catalog, the walk, or both.
    ///
    /// A stale-but-real name beats a static one. The two mechanisms have
    /// different coverage and different freshness, and a user comparing two
    /// machines through `crikey plugin doctor` needs to know which answered.
    fn source_name(&self) -> &'static str {
        match self.answered.load(Ordering::Relaxed) {
            ANSWERED_INDEX => Self::INDEX_SOURCE,
            ANSWERED_WALK => Self::WALK_SOURCE,
            ANSWERED_BOTH => Self::MERGED_SOURCE,
            _ => Self::UNTRIED_SOURCE,
        }
    }

    fn search(&self, query: &FileSearchQuery) -> Result<FileSearchResults> {
        let started = Instant::now();
        let limit = query.limit.min(MAX_FILE_HITS);
        if limit == 0 || query.normalized.is_empty() {
            // Nothing was left unsearched: no name contains the empty query and
            // no caller asked for a hit. Saying `Partial` here would invite the
            // launcher to explain a truncation that did not happen.
            return Ok(FileSearchResults {
                hits: Vec::new(),
                coverage: FileSearchCoverage::Complete,
            });
        }

        // The catalog answered as much as the caller asked for, ran the clock out
        // doing it, was cancelled, or left nowhere to walk. Any of the four makes
        // its answer the whole answer -- and starting a walk after a cancellation
        // would spend the work the cancellation exists to save.
        match self.indexed(query, started, limit) {
            Ok(results) => {
                if results.hits.len() >= limit
                    || results.coverage == FileSearchCoverage::Deadline
                    || results.coverage == FileSearchCoverage::Cancelled
                    || self.roots.is_empty()
                {
                    self.answered.store(ANSWERED_INDEX, Ordering::Relaxed);
                    return Ok(results);
                }

                let walked = self.walk(query, started);
                self.answered.store(ANSWERED_BOTH, Ordering::Relaxed);
                return Ok(merge(results, walked, limit));
            }
            // Nothing indexed and nowhere to walk is not an empty result, it is a
            // search that could not be performed -- and the catalog's own words
            // are the most useful thing to say about it.
            Err(refusal) if self.roots.is_empty() => return Err(refusal),
            Err(_) => {}
        }

        self.answered.store(ANSWERED_WALK, Ordering::Relaxed);
        Ok(self.walk(query, started))
    }
}

/// The catalog's hits, then the walk's, with the walk's duplicates dropped.
///
/// The catalog comes first because it is the broader source and the walk is
/// there to cover what the catalog's configured scope leaves out; a file inside
/// both is one hit, and the deduplication is on the whole path because that is
/// what the launcher opens and derives an item id from. Coverage is the weakest
/// of the two: a deadline or a cancellation on either side bounded the answer,
/// and a cancellation is reported ahead of a deadline because it is the more
/// specific reason the answer is short.
fn merge(indexed: FileSearchResults, walked: FileSearchResults, limit: usize) -> FileSearchResults {
    let coverage = if indexed.coverage == FileSearchCoverage::Cancelled
        || walked.coverage == FileSearchCoverage::Cancelled
    {
        FileSearchCoverage::Cancelled
    } else if indexed.coverage == FileSearchCoverage::Deadline
        || walked.coverage == FileSearchCoverage::Deadline
    {
        FileSearchCoverage::Deadline
    } else {
        FileSearchCoverage::Partial
    };

    let mut hits = indexed.hits;
    // Windows paths are case-insensitive, so the same file reached through the
    // catalog and through a directory read can differ in case and still be one
    // file.
    let mut seen: HashSet<OsString> = hits.iter().map(|hit| lowercase_path(&hit.path)).collect();
    for hit in walked.hits {
        if hits.len() >= limit {
            break;
        }
        if seen.insert(lowercase_path(&hit.path)) {
            hits.push(hit);
        }
    }
    hits.truncate(limit);

    FileSearchResults { hits, coverage }
}

/// A path folded for comparison, without going through `String`.
///
/// Lossy folding would let two genuinely different files -- one whose name is
/// not valid UTF-16 -- collapse into one and vanish from the results, so the
/// key stays an [`OsString`]. ASCII folding is enough for the job: it collapses
/// the drive letter and the directory names Windows itself varies the case of.
fn lowercase_path(path: &PlatformPath) -> OsString {
    let bytes: Vec<u8> = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect();
    // SAFETY: `to_ascii_lowercase` maps ASCII bytes to ASCII bytes and leaves
    // every other byte alone, so the encoding the bytes came in remains valid
    // -- exactly the guarantee `from_encoded_bytes_unchecked` asks for.
    #[allow(unsafe_code)]
    unsafe {
        OsString::from_encoded_bytes_unchecked(bytes)
    }
}

/// The SQL this crate issues against `SystemIndex`, or `None` when there is
/// nothing worth asking.
///
/// The shape is Windows Search SQL, the dialect `ISearchQueryHelper` itself
/// generates and the only one the `Search.CollatorDSO` provider accepts
/// (<https://learn.microsoft.com/en-us/windows/win32/search/-search-3x-wds-sql>).
/// Four decisions are load bearing:
///
/// * **`scope='file:'`** keeps the answer to items with a filesystem path.
///   Without it the catalog also returns mail, OneNote and anything else a
///   protocol handler contributed, none of which a launcher can open by path.
/// * **`System.FileName` only.** Both predicates name that one property, so
///   this is a name query on every platform even though the catalog would
///   happily search contents. `CONTAINS` here is a full-text match against the
///   *file name*, not the file.
/// * **Two predicates, not one.** `LIKE 'query%'` matches a name that starts
///   with the text; `CONTAINS(..., '"query*"')` matches a name any of whose
///   words starts with it, so `log` finds `server log.txt`. A single
///   `LIKE '%query%'` would match more but cannot use the index -- a leading
///   wildcard forces a scan of every indexed name -- and this runs per
///   keystroke. The union is the predicate Flow Launcher ships for the same
///   reason.
/// * **`ORDER BY System.DateModified DESC`** makes the `TOP` clause cut
///   deterministically instead of leaving which matches survive to the
///   provider. Recency rather than `System.Search.Rank` because the launcher
///   ranks names itself and would otherwise have no signal the catalog cannot
///   give it; both are retrievable properties, and rank of a name-only
///   restriction is close to uniform.
pub fn system_index_sql(query: &FileSearchQuery) -> Option<String> {
    let subject = query.normalized.trim();
    let limit = query.limit.min(MAX_FILE_HITS);
    if subject.is_empty() || limit == 0 {
        return None;
    }

    Some(format!(
        "SELECT TOP {limit} {SELECT_COLUMNS} FROM SystemIndex \
         WHERE scope='file:' \
         AND (System.FileName LIKE '{like}%' OR CONTAINS(System.FileName, '\"{phrase}*\"')) \
         ORDER BY System.DateModified DESC",
        like = like_pattern(subject),
        phrase = contains_phrase(subject),
    ))
}

/// The user's text as the operand of a Windows Search `LIKE`.
///
/// Two escapes, both mandatory. A single quote would end the string literal, and
/// doubling it is how SQL says one. `%`, `_` and `[` are `LIKE` metacharacters,
/// and the bracketed character-set form -- `[%]` matches a literal `%` -- is how
/// the dialect spells a literal one, so a user typing `50%` gets files whose
/// name really starts `50%` instead of every file starting `50`.
fn like_pattern(subject: &str) -> String {
    let mut escaped = String::with_capacity(subject.len());
    for character in subject.chars() {
        match character {
            '\'' => escaped.push_str("''"),
            '%' => escaped.push_str("[%]"),
            '_' => escaped.push_str("[_]"),
            '[' => escaped.push_str("[[]"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// The user's text as the phrase inside a `CONTAINS` restriction.
///
/// The phrase sits in double quotes inside a single-quoted SQL literal, so both
/// quote characters have to go: the single quote is doubled as SQL requires, and
/// the double quote becomes a space, which separates words in a full-text phrase
/// and therefore keeps the rest of the query meaningful. No Windows file name
/// can contain a double quote, so nothing findable is lost by it.
fn contains_phrase(subject: &str) -> String {
    let mut escaped = String::with_capacity(subject.len());
    for character in subject.chars() {
        match character {
            '\'' => escaped.push_str("''"),
            '"' => escaped.push(' '),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Seconds since the Unix epoch for a Windows `FILETIME`, given as its count of
/// 100-nanosecond intervals since 1601-01-01 UTC.
///
/// `None` for zero, which is what the catalog carries for an item whose
/// modification time it does not know: the shared contract asks for `None`
/// rather than a fabricated 1601 or 1970, because a missing timestamp must not
/// rank as an ancient one.
pub fn unix_seconds_from_file_time(ticks: u64) -> Option<i64> {
    /// Seconds between 1601-01-01 and 1970-01-01, the two epochs' offset.
    const EPOCH_OFFSET: i64 = 11_644_473_600;
    /// 100-nanosecond intervals in a second.
    const TICKS_PER_SECOND: u64 = 10_000_000;

    if ticks == 0 {
        return None;
    }
    i64::try_from(ticks / TICKS_PER_SECOND)
        .ok()
        .map(|seconds| seconds - EPOCH_OFFSET)
}

/// Seconds since the Unix epoch for a filesystem timestamp.
///
/// Pre-epoch times are real -- an extracted archive can carry one -- and are
/// reported as the negative number they are rather than clamped to zero.
fn unix_seconds(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).ok(),
        Err(before) => i64::try_from(before.duration().as_secs())
            .ok()
            .map(|seconds| -seconds),
    }
}

/// The profile folders the fallback walk visits on this machine.
///
/// `%USERPROFILE%` is set by Windows for every process, which makes it the
/// volume- and username-independent way to reach them without a known-folder
/// call and without hard-coding `C:\Users`.
#[cfg(target_os = "windows")]
fn walk_roots() -> Vec<PathBuf> {
    let Some(profile) = std::env::var_os("USERPROFILE")
        .filter(|profile| !profile.is_empty())
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    WALK_SUBDIRECTORIES
        .iter()
        .map(|name| profile.join(name))
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn walk_roots() -> Vec<PathBuf> {
    Vec::new()
}
