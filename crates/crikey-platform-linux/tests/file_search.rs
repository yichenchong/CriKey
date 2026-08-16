//! File search over real files on Linux (spec 18.1, 18.2).
//!
//! Every case builds a real directory tree in a unique temp directory and
//! searches it, because every property under test is about the filesystem: what
//! a walk descends into, what it refuses to descend into, what it reports for a
//! name the kernel will not let us call a `String`, and what it does when the
//! clock runs out mid-walk. A fixture of `FileHit` values would pin none of
//! that.
//!
//! The index half of the backend is exercised through a stub binary rather than
//! through `plocate` itself. That is deliberate: a suite that only runs where
//! `plocate` is installed pins nothing on CI, and what is CriKey's to get right
//! is the delegation -- the argv it builds, the NUL-separated output it parses,
//! the candidates it refuses to report, the timeout it enforces, and the walk it
//! falls back to. `plocate`'s own correctness is `plocate`'s business.
//!
//! Deliberate non-goals: no test here searches `$HOME` or reads the ambient
//! environment, and none asserts a wall-clock duration as a *performance*
//! claim. The one timing assertion present is a liveness bound -- a search must
//! return, not hang -- which is the contract, not a benchmark.

#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crikey_platform::{
    FileKind, FileSearchCoverage, FileSearchQuery, FileSearchResults, FileSearchService, MAX_FILE_HITS,
};
use crikey_platform_linux::FilesystemSearch;

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
            "crikey-file-search-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak fixtures into this one.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    fn root(&self) -> &Path {
        &self.path
    }

    /// An existing directory at `relative`, parents included.
    fn directory(&self, relative: &str) -> PathBuf {
        let path = self.path.join(relative);
        fs::create_dir_all(&path).expect("fixture directory is creatable");
        path
    }

    /// An empty file at `relative`, parents included.
    fn file(&self, relative: &str) -> PathBuf {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent is creatable");
        }
        fs::write(&path, b"fixture").expect("fixture file is writable");
        path
    }

    /// A file whose name is not valid UTF-8.
    ///
    /// A perfectly legal Linux filename: the kernel stores bytes, and a
    /// launcher that cannot represent one cannot open it either (spec 19.2).
    fn file_with_raw_name(&self, directory: &str, name: &[u8]) -> PathBuf {
        let parent = self.directory(directory);
        let path = parent.join(OsStr::from_bytes(name));
        fs::write(&path, b"fixture").expect("fixture file with raw name is writable");
        path
    }

    /// An executable stand-in for `plocate` that prints `script` verbatim.
    fn stub_locate(&self, name: &str, script: &str) -> PathBuf {
        let path = self.path.join(name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&path)
            .expect("stub binary is creatable");
        file.write_all(script.as_bytes())
            .expect("stub binary is writable");

        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A query with a budget generous enough that only the fixture's size decides
/// when the search ends.
fn query(text: &str, limit: usize) -> FileSearchQuery {
    FileSearchQuery {
        normalized: text.to_owned(),
        limit,
        deadline: Duration::from_secs(30),
    }
}

/// The basenames of a result set, sorted.
///
/// Sorted because `readdir` order is a filesystem detail and no test here is
/// about it: the walk's *breadth-first* order is what bounds a deadline-cut
/// answer to shallow entries, and that is not the same claim as the order two
/// entries of one directory come back in.
fn names(results: &FileSearchResults) -> Vec<String> {
    let mut names: Vec<String> = results.hits.iter().map(|hit| hit.name.clone()).collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// The walk (spec 18.1)
// ---------------------------------------------------------------------------

/// A file is found by a fragment of its basename, whatever its depth.
///
/// The elementary contract, and it also pins the match subject: the query
/// appears in a *parent directory* name of a second file, which must not turn
/// that file into a hit. Matching the whole path would rank a deeply buried
/// stranger alongside the file the user named.
#[test]
fn a_file_is_found_by_a_fragment_of_its_basename_and_not_by_its_parents() {
    let scratch = Scratch::new();
    scratch.file("projects/quarterly-report.md");
    scratch.file("report-archive/unrelated.txt");

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let results = service.search(&query("report", 32)).expect("the walk answers");

    assert_eq!(
        names(&results),
        vec!["quarterly-report.md".to_owned(), "report-archive".to_owned()],
        "only the entries whose own basename contains the query may be reported"
    );
    assert_eq!(
        results.coverage,
        FileSearchCoverage::Complete,
        "a walk that drained a readable fixture covers everything it was configured to see"
    );
}

/// Case is folded, in both directions.
#[test]
fn matching_folds_case_in_both_directions() {
    let scratch = Scratch::new();
    scratch.file("Invoice-2026.PDF");

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    for text in ["invoice", "INVOICE", "iNvOiCe"] {
        let results = service.search(&query(text, 32)).expect("the walk answers");
        assert_eq!(
            names(&results),
            vec!["Invoice-2026.PDF".to_owned()],
            "the query {text:?} must fold against the stored name"
        );
    }
}

/// A directory is reported as one, and so is a symlink that resolves to one.
///
/// Kills the bug where everything comes back as `FileKind::File`: the launcher
/// ranks a directory above a file and opens it differently, so a folder
/// reported as a file is both mis-ranked and mis-activated. The symlink half is
/// the same contract seen from the side the walk refuses to descend: it is not
/// a way into the tree, but it is still a folder to the user.
#[test]
fn a_directory_is_reported_as_a_directory_and_so_is_a_link_to_one() {
    let scratch = Scratch::new();
    scratch.directory("ledger-folder");
    scratch.file("ledger-file.txt");
    std::os::unix::fs::symlink(
        scratch.root().join("ledger-folder"),
        scratch.root().join("ledger-link"),
    )
    .expect("symlink is creatable");

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let results = service.search(&query("ledger", 32)).expect("the walk answers");

    let mut kinds: Vec<(String, FileKind)> = results
        .hits
        .iter()
        .map(|hit| (hit.name.clone(), hit.kind))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            ("ledger-file.txt".to_owned(), FileKind::File),
            ("ledger-folder".to_owned(), FileKind::Directory),
            ("ledger-link".to_owned(), FileKind::Directory),
        ],
        "kind must follow what the entry resolves to, not what walking it would cost"
    );
}

/// Hidden, `.git` and `node_modules` trees are reported but never entered.
///
/// Kills the bug that makes the whole feature useless on a developer's machine:
/// a walk that descends `~/.cache` and `node_modules` spends the entire
/// keystroke budget on files nobody launches and returns before it reaches the
/// documents. The excluded directory itself stays findable -- excluding it is
/// about not paying for its contents, not about hiding it.
#[test]
fn excluded_directories_are_reported_but_never_descended() {
    let scratch = Scratch::new();
    scratch.file(".cache/target-inside-hidden.txt");
    scratch.file(".git/target-inside-git.txt");
    scratch.file("node_modules/target-inside-modules.txt");
    scratch.file("documents/target-inside-documents.txt");
    scratch.directory("node_modules-of-mine");
    scratch.file("node_modules-of-mine/target-inside-lookalike.txt");

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let results = service
        .search(&query("target-inside", 64))
        .expect("the walk answers");

    assert_eq!(
        names(&results),
        vec![
            "target-inside-documents.txt".to_owned(),
            "target-inside-lookalike.txt".to_owned(),
        ],
        "only trees the walk is allowed to enter may contribute hits, and a directory whose name \
         merely starts with an excluded one is not excluded"
    );

    let excluded = service
        .search(&query("node_modules", 64))
        .expect("the walk answers");
    assert_eq!(
        names(&excluded),
        vec!["node_modules".to_owned(), "node_modules-of-mine".to_owned()],
        "an excluded directory is still a findable directory; only its contents are skipped"
    );
}

/// A symlink loop terminates.
///
/// Any user can create `~/loop -> ~` by accident. A walk that follows symlinks
/// without a visited set never returns, and "never returns" on a keystroke is
/// the worst failure this backend has available.
#[test]
fn a_symlink_that_points_at_its_own_ancestor_does_not_trap_the_walk() {
    let scratch = Scratch::new();
    scratch.file("documents/looped-target.txt");
    std::os::unix::fs::symlink(scratch.root(), scratch.root().join("documents/loop"))
        .expect("symlink is creatable");

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let started = Instant::now();
    let results = service
        .search(&query("looped-target", 8))
        .expect("the walk answers");

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the walk must not follow a loop; it took {:?}",
        started.elapsed()
    );
    assert_eq!(
        names(&results),
        vec!["looped-target.txt".to_owned()],
        "the file behind the loop is still found exactly once"
    );
}

/// A non-UTF-8 filename survives into `PlatformPath` byte for byte.
///
/// Kills the bug where a name is round-tripped through `String`: a lossy
/// conversion replaces the offending byte with U+FFFD, and the resulting path
/// names a file that does not exist, so activating the row fails (spec 19.2).
#[test]
fn a_non_utf8_filename_survives_into_the_reported_path_losslessly() {
    let scratch = Scratch::new();
    // `caf\xE9-report.txt`: Latin-1 e-acute, which is not valid UTF-8.
    let raw = b"caf\xE9-report.txt";
    let expected = scratch.file_with_raw_name("documents", raw);

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let results = service.search(&query("report", 8)).expect("the walk answers");

    let hit = results
        .hits
        .iter()
        .find(|hit| hit.path.as_path() == expected)
        .expect("the file with a non-UTF-8 name is found by the ASCII part of its name");
    assert_eq!(
        hit.path.as_os_str().as_bytes(),
        expected.as_os_str().as_bytes(),
        "the reported path must be the kernel's bytes, not a lossy rendering of them"
    );
    assert!(
        hit.path.as_path().exists(),
        "a path that cannot be opened is not a usable hit"
    );
    assert!(
        hit.name.contains('\u{fffd}'),
        "the display name is the lossy one -- that is what `name` is for -- while the path is not"
    );
}

/// A hit carries a modification time, and it is not a fabricated zero.
#[test]
fn a_hit_carries_the_modification_time_the_filesystem_reports() {
    let scratch = Scratch::new();
    scratch.file("documents/dated-note.txt");

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let results = service.search(&query("dated-note", 8)).expect("the walk answers");

    let seconds = results
        .hits
        .first()
        .expect("the note is found")
        .modified_unix_seconds;
    // 2020-01-01 is comfortably before any run of this suite and comfortably
    // after any plausible fabricated epoch value.
    assert!(
        seconds.is_some_and(|seconds| seconds > 1_577_836_800),
        "a file written moments ago must not be reported as unstamped or as modified in 1970: {seconds:?}"
    );
}

// ---------------------------------------------------------------------------
// Bounds: the limit and the deadline
// ---------------------------------------------------------------------------

/// The caller's limit stops the walk, and truncation is not called complete.
#[test]
fn the_callers_limit_is_honoured_and_a_truncated_answer_says_so() {
    let scratch = Scratch::new();
    for index in 0..40 {
        scratch.file(&format!("documents/bounded-{index:02}.txt"));
    }

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let results = service.search(&query("bounded-", 5)).expect("the walk answers");

    assert_eq!(
        results.hits.len(),
        5,
        "the caller asked for five hits and gets five"
    );
    assert_eq!(
        results.coverage,
        FileSearchCoverage::Partial,
        "a walk cut short by the limit has provably more to give and must not claim completeness"
    );
}

/// `MAX_FILE_HITS` clamps a caller that asks for more.
///
/// Kills the bug where the limit is trusted verbatim: the cap exists so that
/// neither a broken caller nor a query matching a million files can make the
/// host allocate and rank an unbounded answer.
#[test]
fn a_limit_beyond_the_maximum_is_clamped_to_it() {
    let scratch = Scratch::new();
    for index in 0..(MAX_FILE_HITS + 8) {
        scratch.file(&format!("documents/capped-{index:04}.txt"));
    }

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let results = service
        .search(&query("capped-", usize::MAX))
        .expect("the walk answers");

    assert_eq!(
        results.hits.len(),
        MAX_FILE_HITS,
        "no caller may be handed more than MAX_FILE_HITS, whatever it asks for"
    );
    assert_eq!(
        results.coverage,
        FileSearchCoverage::Partial,
        "the cap truncated the answer, so it does not cover everything"
    );
}

/// An expired budget returns immediately, and says the clock is why.
///
/// Kills the bug where the deadline is checked only after a directory is fully
/// read, or not at all: a walk of a real home directory takes seconds, and a
/// launcher that blocks a keystroke for seconds is unusable. `Deadline` rather
/// than an error, and rather than `Partial`, because "a later identical query
/// may return more" is a different fact about the answer (spec 18.1).
#[test]
fn an_expired_deadline_returns_at_once_and_reports_the_clock_as_the_reason() {
    let scratch = Scratch::new();
    // Wide and deep enough that no host finishes it in zero time.
    for outer in 0..8 {
        for inner in 0..40 {
            scratch.file(&format!("tree-{outer}/deep/deadline-{inner:02}.txt"));
        }
    }

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let expired = FileSearchQuery {
        normalized: "deadline-".to_owned(),
        limit: 64,
        deadline: Duration::ZERO,
    };

    let started = Instant::now();
    let results = service
        .search(&expired)
        .expect("an expired budget is not an error");
    let elapsed = started.elapsed();

    assert_eq!(
        results.coverage,
        FileSearchCoverage::Deadline,
        "the clock, not the fixture, ended this search"
    );
    assert!(
        results.hits.is_empty(),
        "no budget means no time in which to find anything"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "a search with no budget must return at once; it took {elapsed:?}"
    );

    // The same fixture with a real budget proves the fixture was searchable and
    // that the zero-budget answer above was a decision rather than a failure.
    let unhurried = service.search(&query("deadline-", 64)).expect("the walk answers");
    assert_eq!(
        unhurried.hits.len(),
        64,
        "the fixture really does hold matching files"
    );
}

/// A tiny budget over a large fixture returns quickly and honestly.
///
/// The zero-budget case above pins the pre-flight check; this one pins the
/// in-flight one, where the walk is already inside `read_dir` results when the
/// clock runs out. The assertion is on the *bound*, never on the exact
/// coverage: a fast host may legitimately finish this fixture inside a
/// millisecond, and pinning `Deadline` here would be a flake, not a contract.
#[test]
fn a_tiny_budget_over_a_large_fixture_returns_promptly_without_hanging() {
    let scratch = Scratch::new();
    for outer in 0..12 {
        for inner in 0..120 {
            scratch.file(&format!("tree-{outer:02}/tiny-{inner:03}.txt"));
        }
    }

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    let hurried = FileSearchQuery {
        normalized: "tiny-".to_owned(),
        limit: MAX_FILE_HITS,
        deadline: Duration::from_millis(1),
    };

    let started = Instant::now();
    let results = service.search(&hurried).expect("a tiny budget is not an error");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "a one-millisecond budget must not turn into a one-second walk; it took {elapsed:?}"
    );
    assert!(
        results.hits.len() <= MAX_FILE_HITS,
        "the answer stays bounded however the walk ended"
    );
    assert_ne!(
        results.coverage,
        FileSearchCoverage::Complete,
        "1440 matches against a limit of {MAX_FILE_HITS} cannot be a complete answer however fast \
         the host is, so this pins the bound without pinning the clock"
    );
}

/// An empty query matches nothing rather than everything.
///
/// A substring test against an empty needle matches every entry, which would
/// hand the host a screenful of arbitrary files on the keystroke that clears
/// the input box.
#[test]
fn an_empty_query_matches_nothing() {
    let scratch = Scratch::new();
    scratch.file("documents/anything.txt");

    let service = FilesystemSearch::walking(vec![scratch.root().to_path_buf()]);
    for text in ["", "   "] {
        let results = service
            .search(&query(text, 32))
            .expect("an empty query is not an error");
        assert!(
            results.hits.is_empty(),
            "the query {text:?} names no file and must match none"
        );
    }
}

/// A service with no roots answers empty, and does not claim to have looked.
#[test]
fn a_service_with_no_roots_answers_empty_and_never_claims_completeness() {
    let service = FilesystemSearch::walking(Vec::new());
    let results = service
        .search(&query("anything", 32))
        .expect("no roots is not an error");

    assert!(results.hits.is_empty(), "there is nothing to search");
    assert_eq!(
        results.coverage,
        FileSearchCoverage::Partial,
        "an empty answer from a service that searched nothing must not be called complete"
    );
}

/// A root that does not exist degrades to a partial answer, not an error.
///
/// A configured root can be an unmounted removable disk. That is a coverage
/// fact, not a failure: the other roots still have answers, and refusing to
/// answer at all would teach the user the feature is broken (spec 18.2).
#[test]
fn an_unreadable_root_lowers_coverage_instead_of_failing_the_search() {
    let scratch = Scratch::new();
    scratch.file("documents/mounted-note.txt");

    let service = FilesystemSearch::walking(vec![
        scratch.root().to_path_buf(),
        scratch.root().join("not-mounted"),
    ]);
    let results = service
        .search(&query("mounted-note", 8))
        .expect("the walk answers");

    assert_eq!(
        names(&results),
        vec!["mounted-note.txt".to_owned()],
        "the readable root still answers"
    );
    assert_eq!(
        results.coverage,
        FileSearchCoverage::Partial,
        "a root that could not be read is missing from the answer and must be admitted"
    );
}

// ---------------------------------------------------------------------------
// Delegating to the index (spec 18.1)
// ---------------------------------------------------------------------------

/// The index's answer is parsed, filtered to the roots, and never called
/// complete.
///
/// Three contracts in one delegation. NUL separation is what makes the output
/// unambiguous for filenames containing newlines, so the parse must use it; a
/// candidate outside the configured roots is not this service's to report,
/// or the same query answers differently depending on whether `plocate` is
/// installed; and an index rebuilt by a timer cannot back a `Complete` claim.
#[test]
fn an_index_answer_is_parsed_filtered_to_the_roots_and_reported_as_partial() {
    let scratch = Scratch::new();
    let inside = scratch.file("documents/indexed-note.txt");
    let hidden = scratch.file(".cache/indexed-hidden.txt");
    let stale = scratch.root().join("documents/indexed-deleted.txt");
    let outside = Scratch::new();
    let elsewhere = outside.file("indexed-elsewhere.txt");

    let stub = scratch.stub_locate(
        "stub-locate",
        &format!(
            "#!/bin/sh\nprintf '%s\\0' '{}' '{}' '{}' '{}'\n",
            inside.display(),
            hidden.display(),
            stale.display(),
            elsewhere.display()
        ),
    );

    let service = FilesystemSearch::with_locate(vec![scratch.root().to_path_buf()], stub);
    assert_eq!(
        service.source_name(),
        "plocate",
        "the diagnostic must name the index when one is in front of the walk"
    );

    let results = service
        .search(&query("indexed-", 32))
        .expect("the delegation answers");

    assert_eq!(
        names(&results),
        vec!["indexed-note.txt".to_owned()],
        "a candidate outside the roots, one inside a directory the walk would not descend, and one \
         the index is merely stale about are all unreportable"
    );
    assert_eq!(
        results.coverage,
        FileSearchCoverage::Partial,
        "an index is only as fresh as the last updatedb, so its answer never covers everything"
    );
}

/// The index is asked for a basename match, case-insensitively, and never
/// through a shell.
///
/// Kills two bugs. Without `--basename` the index matches directory names in
/// the path and the two mechanisms answer different questions; without `--` a
/// query the user starts with a dash becomes an option and the delegation
/// fails on ordinary input.
#[test]
fn the_index_is_invoked_for_a_case_insensitive_basename_match_with_the_pattern_after_a_separator() {
    let scratch = Scratch::new();
    let recorded = scratch.root().join("argv.txt");
    let target = scratch.file("documents/-dashed-name.txt");
    let stub = scratch.stub_locate(
        "stub-locate",
        &format!(
            "#!/bin/sh\nfor argument in \"$@\"; do printf '%s\\n' \"$argument\" >> '{}'; done\nprintf '%s\\0' '{}'\n",
            recorded.display(),
            target.display()
        ),
    );

    let service = FilesystemSearch::with_locate(vec![scratch.root().to_path_buf()], stub);
    let results = service
        .search(&query("-dashed-name", 32))
        .expect("the delegation answers");
    assert_eq!(
        names(&results),
        vec!["-dashed-name.txt".to_owned()],
        "a query starting with a dash must reach the index as a pattern"
    );

    let argv: Vec<String> = fs::read_to_string(&recorded)
        .expect("the stub recorded its arguments")
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(
        argv.contains(&"--ignore-case".to_owned()) && argv.contains(&"--basename".to_owned()),
        "the index must be asked the same question the walk answers: {argv:?}"
    );
    assert!(
        argv.contains(&"--null".to_owned()),
        "output must be NUL separated so a filename with a newline stays one candidate: {argv:?}"
    );
    let separator = argv
        .iter()
        .position(|argument| argument == "--")
        .expect("the pattern is introduced by a separator");
    assert_eq!(
        argv.get(separator + 1).map(String::as_str),
        Some("-dashed-name"),
        "the pattern must be the argument right after `--`: {argv:?}"
    );
}

/// An index that fails falls back to the walk instead of answering empty.
///
/// `plocate` exits non-zero when its database is missing, which is the normal
/// state on a machine where `updatedb` has never run. Treating that as "no
/// matches" would make file search silently useless on exactly those machines.
#[test]
fn an_index_that_fails_falls_back_to_the_walk() {
    let scratch = Scratch::new();
    scratch.file("documents/fallback-note.txt");
    let stub = scratch.stub_locate("stub-locate", "#!/bin/sh\necho 'no database' >&2\nexit 1\n");

    let service = FilesystemSearch::with_locate(vec![scratch.root().to_path_buf()], stub);
    let results = service
        .search(&query("fallback-note", 8))
        .expect("the fallback answers");

    assert_eq!(
        names(&results),
        vec!["fallback-note.txt".to_owned()],
        "a broken index must not cost the user the walk that still works"
    );
}

/// A missing index binary falls back to the walk.
#[test]
fn an_index_binary_that_cannot_be_spawned_falls_back_to_the_walk() {
    let scratch = Scratch::new();
    scratch.file("documents/spawnless-note.txt");

    let service = FilesystemSearch::with_locate(
        vec![scratch.root().to_path_buf()],
        PathBuf::from("/nonexistent/plocate"),
    );
    let results = service
        .search(&query("spawnless-note", 8))
        .expect("the fallback answers");

    assert_eq!(
        names(&results),
        vec!["spawnless-note.txt".to_owned()],
        "a binary that cannot even be spawned must not end the search"
    );
}

/// An index that hangs is killed, and the deadline still holds.
///
/// Kills the bug that makes delegation worse than no delegation: a child read
/// to EOF with no timeout blocks the keystroke for as long as the child feels
/// like living. The budget here is a small fraction of the stub's sleep, so the
/// only way to pass is to stop waiting and move on.
#[test]
fn an_index_that_hangs_is_abandoned_within_the_deadline() {
    let scratch = Scratch::new();
    scratch.file("documents/patient-note.txt");
    let stub = scratch.stub_locate("stub-locate", "#!/bin/sh\nsleep 30\n");

    let service = FilesystemSearch::with_locate(vec![scratch.root().to_path_buf()], stub);
    let hurried = FileSearchQuery {
        normalized: "patient-note".to_owned(),
        limit: 8,
        deadline: Duration::from_millis(300),
    };

    let started = Instant::now();
    let results = service.search(&hurried).expect("a hanging index is not an error");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "the delegation must be abandoned, not waited out; it took {elapsed:?}"
    );
    assert!(
        matches!(
            results.coverage,
            FileSearchCoverage::Deadline | FileSearchCoverage::Partial | FileSearchCoverage::Complete
        ),
        "whatever coverage it reports, the search must return an answer rather than an error"
    );
}
