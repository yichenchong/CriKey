//! Searching the user's files and folders (spec 18.1 "File search", 18.2).
//!
//! # Why this is a separate trait
//!
//! For the same reason [`WindowService`](crate::WindowService) is: the answer
//! is per-session, not per-build. Windows can hand the query to an index the
//! operating system already maintains; macOS can too, unless the user has
//! turned Spotlight off or withheld the folder through TCC; a Linux session may
//! have a desktop indexer, or nothing at all. Folding these methods into
//! [`PlatformBackend`](crate::PlatformBackend) would force every backend to
//! answer, and an unwilling backend can only answer with a lie.
//!
//! A backend therefore hands out a [`FileSearchService`] only when it has one,
//! and reports [`Capability::FileSearch`](crate::Capability::FileSearch) for
//! the session it is actually running in. The distinction between the states
//! is the user-facing part: `Unavailable` means this build cannot search files
//! here, `PermissionGated` means the user could grant it, and `Partial` means
//! results are real but do not cover everything.
//!
//! # What implementations owe the caller
//!
//! * **A deadline is a promise, not a hint.** Search runs on a keystroke. An
//!   implementation that cannot answer within [`FileSearchQuery::deadline`]
//!   returns what it has; it never blocks past it, and never returns an error
//!   merely because it ran out of time.
//! * **Partial truth beats silence.** A backend that can see half the user's
//!   files reports [`CapabilityState::Partial`](crate::CapabilityState::Partial)
//!   and returns that half. Refusing to answer teaches the user the feature is
//!   broken; answering narrowly teaches them where it looks.
//! * **Paths are lossless.** A hit carries a [`PlatformPath`], never a
//!   `String`, because a launcher that cannot represent a filename cannot open
//!   it either (spec 19.2).
//! * **No content search.** This interface is about names and locations. An
//!   implementation backed by a service that also indexes content must still
//!   only answer name queries through it, so results mean the same thing on
//!   every platform.

use std::time::Duration;

use crikey_core::{PlatformPath, Result};

/// Largest number of hits a caller may ask one search for.
///
/// The launcher shows a screenful and ranks what it is given, so a larger
/// answer costs decode and ranking time for rows nobody scrolls to. It also
/// bounds what a hostile or broken index can make the host allocate in one
/// call.
pub const MAX_FILE_HITS: usize = 512;

/// Whether a hit is a file or the folder containing one.
///
/// Kept distinct because the launcher ranks them differently — a directory is
/// weighted above a file (`crikey-ranking`) on the theory that a user typing a
/// folder's name usually wants to go there, not to open something inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileKind {
    File,
    Directory,
}

/// One file or folder a backend found.
///
/// `name` is the basename as the filesystem spells it, and is what the matcher
/// scores against; `path` is the whole location and is what gets opened.
/// Splitting them is not redundancy: scoring against the full path would let a
/// deeply buried file outrank an exact basename match because the query
/// happened to appear in one of its parent directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    pub name: String,
    pub path: PlatformPath,
    pub kind: FileKind,
    /// Last modification time as seconds since the Unix epoch, when the
    /// backend can supply it cheaply. `None` rather than a fabricated zero: a
    /// missing timestamp must not rank as "modified in 1970".
    pub modified_unix_seconds: Option<i64>,
}

/// One search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchQuery {
    /// The user's text, already trimmed and lowercased by the caller so every
    /// backend matches against the same subject.
    pub normalized: String,
    /// Upper bound on returned hits; a backend clamps this to [`MAX_FILE_HITS`].
    pub limit: usize,
    /// How long the caller will wait. See the module note: this is a promise.
    pub deadline: Duration,
}

/// How completely a backend's answer covers the user's files.
///
/// Reported per search rather than per session because coverage can change
/// under the backend's feet — an index can be rebuilding, a permission can be
/// granted mid-session. The launcher surfaces this so a thin answer is
/// explicable rather than mysterious.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSearchCoverage {
    /// Everything the backend is configured to see was searched.
    Complete,
    /// Real results, but from a subset: an index still building, a scope the
    /// user has narrowed, or folders the process may not read.
    Partial,
    /// The deadline expired before the search finished. Results are whatever
    /// had been found; a later identical query may return more.
    Deadline,
}

/// One search's results and how much of the filesystem stood behind them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchResults {
    pub hits: Vec<FileHit>,
    pub coverage: FileSearchCoverage,
}

/// Searching the user's files and folders by name.
pub trait FileSearchService: Send + Sync {
    /// A short, stable name for whatever is actually answering — the index or
    /// mechanism, not the operating system.
    ///
    /// This is a diagnostic, printed by `crikey plugin doctor` and worth
    /// having because the same platform can answer through different means:
    /// on Windows the system index and a directory walk give different
    /// coverage and different freshness, and a user comparing two machines
    /// needs to know which one answered.
    fn source_name(&self) -> &'static str;
    /// Files and folders whose name matches `query`.
    ///
    /// An error means the search could not be performed at all. A search that
    /// legitimately found nothing returns empty hits, and one that ran out of
    /// time returns [`FileSearchCoverage::Deadline`] — neither is an error,
    /// and both are ordinary on a launcher's hot path.
    fn search(&self, query: &FileSearchQuery) -> Result<FileSearchResults>;
}
