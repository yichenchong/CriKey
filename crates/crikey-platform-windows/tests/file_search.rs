//! Public-API contract for Windows file search (spec 18.1, 18.2).
//!
//! Two halves are pinned here, and both run on every host.
//!
//! The first is the `SystemIndex` query string. Nothing about it can be checked
//! by reading it on target -- a mis-escaped quote returns the wrong files rather
//! than an error, and a predicate that reaches the file's *contents* instead of
//! its name would breach the shared contract silently and only on machines whose
//! catalog indexes contents. So the string is built by a pure function and its
//! exact shape is asserted here.
//!
//! The second is the fallback walk, which is what a default-configured Windows
//! machine relies on for the folders Classic indexing mode leaves out. It is
//! ordinary `std::fs`, so it is exercised against real fixture directories on
//! whatever host the suite is running on.
//!
//! What cannot be pinned without a Windows kernel -- that the Search service
//! activates, that the OLE DB provider accepts this SQL, that the row bindings
//! match the columns the catalog returns -- is deliberately not claimed here.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(target_os = "windows"))]
use crikey_core::CoreError;
use crikey_platform::{
    CancelToken, FileKind, FileSearchCoverage, FileSearchQuery, FileSearchService, MAX_FILE_HITS,
};
use crikey_platform_windows::{
    system_index_sql, unix_seconds_from_file_time, WindowsFileSearch, SELECT_COLUMNS, WALK_SUBDIRECTORIES,
};

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
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    /// An existing directory inside the scratch directory, parents included.
    fn directory(&self, relative: &str) -> PathBuf {
        let directory = self.path.join(relative);
        fs::create_dir_all(&directory).expect("fixture directory is creatable");
        directory
    }

    /// An existing empty file inside the scratch directory, parents included.
    fn file(&self, relative: &str) -> PathBuf {
        let file = self.path.join(relative);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).expect("fixture parent is creatable");
        }
        fs::write(&file, b"").expect("fixture file is writable");
        file
    }

    /// A scratch path that is deliberately never created.
    fn missing(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A query with a generous deadline, which is what every case that is not about
/// the deadline wants.
fn query(text: &str, limit: usize) -> FileSearchQuery {
    FileSearchQuery {
        normalized: text.to_owned(),
        limit,
        deadline: Duration::from_secs(30),
        cancel: CancelToken::new(),
    }
}

// ---------------------------------------------------------------------------
// The SystemIndex query

#[test]
fn the_index_query_restricts_to_files_and_matches_names_two_ways() {
    let sql = system_index_sql(&query("report", 20)).expect("a real query produces SQL");

    assert_eq!(
        sql,
        "SELECT TOP 20 System.ItemPathDisplay, System.FileName, System.ItemType, \
         System.FileAttributes, System.DateModified FROM SystemIndex \
         WHERE scope='file:' \
         AND (System.FileName LIKE 'report%' OR CONTAINS(System.FileName, '\"report*\"')) \
         ORDER BY System.DateModified DESC"
    );
}

#[test]
fn the_index_query_selects_every_column_a_hit_needs() {
    let sql = system_index_sql(&query("report", 20)).expect("a real query produces SQL");

    assert!(sql.contains(SELECT_COLUMNS), "the select list is shared");
    for column in [
        // The path is what gets opened, the name is what gets scored, the type
        // and attributes decide file versus folder, the date is the timestamp.
        "System.ItemPathDisplay",
        "System.FileName",
        "System.ItemType",
        "System.FileAttributes",
        "System.DateModified",
    ] {
        assert!(sql.contains(column), "{column} must be selected: {sql}");
    }
}

/// The shared contract says this interface is about names, so a query that can
/// reach a file's contents is a breach whatever it returns.
#[test]
fn the_index_query_never_searches_contents() {
    let sql = system_index_sql(&query("report", 20)).expect("a real query produces SQL");

    for forbidden in ["System.Search.Contents", "FREETEXT", "System.Search.AutoSummary"] {
        assert!(
            !sql.contains(forbidden),
            "{forbidden} would search contents, not names: {sql}"
        );
    }
    // Both restrictions name the one property, so `CONTAINS` here is a full-text
    // match against the file's name and not against the file.
    assert_eq!(
        sql.matches("System.FileName").count(),
        3,
        "the select list and both restrictions, and nothing else: {sql}"
    );
}

#[test]
fn the_index_query_clamps_the_limit_to_the_shared_maximum() {
    let sql = system_index_sql(&query("report", MAX_FILE_HITS * 10)).expect("a real query produces SQL");

    assert!(
        sql.starts_with(&format!("SELECT TOP {MAX_FILE_HITS} ")),
        "a caller asking for more than the contract allows gets the maximum: {sql}"
    );
}

#[test]
fn an_empty_query_asks_the_index_nothing() {
    assert!(system_index_sql(&query("", 20)).is_none());
    assert!(
        system_index_sql(&query("   ", 20)).is_none(),
        "whitespace is not a name to search for"
    );
    assert!(
        system_index_sql(&query("report", 0)).is_none(),
        "a caller wanting no hits does not need a query issued"
    );
}

/// An unescaped quote would end the SQL string literal, which is the one
/// escaping bug in a query builder that a reviewer cannot see and a user can
/// trigger by typing an apostrophe.
#[test]
fn a_quote_in_the_query_is_doubled_in_both_restrictions() {
    let sql = system_index_sql(&query("o'brien", 20)).expect("a real query produces SQL");

    assert!(
        sql.contains("LIKE 'o''brien%'"),
        "the LIKE operand must double the quote: {sql}"
    );
    assert!(
        sql.contains("CONTAINS(System.FileName, '\"o''brien*\"')"),
        "so must the CONTAINS phrase: {sql}"
    );
}

/// `%`, `_` and `[` are `LIKE` metacharacters. A user typing `50%` wants files
/// whose name starts with `50%`, not every file whose name starts with `50`.
#[test]
fn like_metacharacters_in_the_query_match_themselves() {
    let sql = system_index_sql(&query("50%_[x", 20)).expect("a real query produces SQL");

    assert!(
        sql.contains("LIKE '50[%][_][[]x%'"),
        "each metacharacter becomes a literal character set, and only the \
         trailing wildcard stays a wildcard: {sql}"
    );
    // The full-text phrase has no such metacharacters, so it keeps the text.
    assert!(
        sql.contains("CONTAINS(System.FileName, '\"50%_[x*\"')"),
        "the CONTAINS phrase is not a LIKE pattern: {sql}"
    );
}

/// A double quote would close the full-text phrase and leave the rest of the
/// query as loose operators. No Windows file name can contain one, so turning it
/// into a word separator loses nothing findable.
#[test]
fn a_double_quote_cannot_break_out_of_the_contains_phrase() {
    let sql = system_index_sql(&query("say \"hi\"", 20)).expect("a real query produces SQL");

    assert!(
        sql.contains("CONTAINS(System.FileName, '\"say  hi *\"')"),
        "each double quote becomes a space: {sql}"
    );
    let (_, phrase) = sql
        .split_once("CONTAINS(")
        .expect("the CONTAINS restriction is present");
    assert_eq!(
        phrase.matches('"').count(),
        2,
        "inside the restriction, exactly the two quotes that delimit the phrase \
         remain: {sql}"
    );
    // The `LIKE` operand keeps the character, because there it is an ordinary
    // literal rather than a delimiter: it simply matches no file, since no
    // Windows file name can contain a double quote.
    assert!(
        sql.contains("LIKE 'say \"hi\"%'"),
        "the LIKE operand is left alone: {sql}"
    );
}

// ---------------------------------------------------------------------------
// FILETIME arithmetic

#[test]
fn the_windows_epoch_is_converted_to_the_unix_one() {
    /// 100-nanosecond intervals between 1601-01-01 and 1970-01-01.
    const UNIX_EPOCH_TICKS: u64 = 11_644_473_600 * 10_000_000;

    assert_eq!(unix_seconds_from_file_time(UNIX_EPOCH_TICKS), Some(0));
    // 2001-09-09T01:46:40Z, a round Unix second, so a sign or offset error moves
    // the answer by decades rather than by a rounding.
    assert_eq!(
        unix_seconds_from_file_time(UNIX_EPOCH_TICKS + 1_000_000_000 * 10_000_000),
        Some(1_000_000_000)
    );
    // Before the Unix epoch but after the Windows one: real, and negative.
    assert_eq!(unix_seconds_from_file_time(10_000_000), Some(-11_644_473_599));
}

/// A missing timestamp must not rank as "modified in 1601" or "in 1970": the
/// shared contract asks for `None`.
#[test]
fn an_unset_file_time_has_no_timestamp() {
    assert_eq!(unix_seconds_from_file_time(0), None);
}

// ---------------------------------------------------------------------------
// The fallback walk

#[test]
fn the_walk_reports_files_and_folders_it_finds_by_name() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    let file = scratch.file("root/quarterly report.txt");
    let folder = scratch.directory("root/Reports");
    scratch.file("root/unrelated.txt");

    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search.walk(&query("report", 50), Instant::now());

    let mut found: Vec<(String, FileKind)> = results
        .hits
        .iter()
        .map(|hit| (hit.name.clone(), hit.kind))
        .collect();
    found.sort();
    assert_eq!(
        found,
        vec![
            ("Reports".to_owned(), FileKind::Directory),
            ("quarterly report.txt".to_owned(), FileKind::File),
        ],
        "the match is case insensitive, and a folder is reported as one"
    );

    let paths: Vec<_> = results
        .hits
        .iter()
        .map(|hit| hit.path.as_path().to_path_buf())
        .collect();
    assert!(paths.contains(&file), "the whole path is what gets opened");
    assert!(paths.contains(&folder));
    assert!(
        results.hits.iter().any(|hit| hit.modified_unix_seconds.is_some()),
        "a file just written has a modification time the walk can read"
    );
}

/// These roots are a handful of profile folders, never the filesystem, so an
/// answer from them is never complete however much time it had.
#[test]
fn the_walk_never_claims_complete_coverage() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    scratch.file("root/report.txt");

    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search.walk(&query("report", 50), Instant::now());

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.coverage, FileSearchCoverage::Partial);
}

#[test]
fn the_walk_stops_at_the_limit() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    for index in 0..40 {
        scratch.file(&format!("root/report-{index}.txt"));
    }

    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search.walk(&query("report", 7), Instant::now());

    assert_eq!(results.hits.len(), 7, "the caller's limit is a bound");
    assert_eq!(results.coverage, FileSearchCoverage::Partial);
}

/// A deadline is a promise: an expired one is reported, not an error and not a
/// walk that runs to completion anyway.
#[test]
fn an_expired_deadline_is_reported_rather_than_overrun() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    scratch.file("root/report.txt");

    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search.walk(
        &FileSearchQuery {
            normalized: "report".to_owned(),
            limit: 50,
            deadline: Duration::ZERO,
            cancel: CancelToken::new(),
        },
        Instant::now(),
    );

    assert_eq!(results.coverage, FileSearchCoverage::Deadline);
    assert!(results.hits.is_empty(), "no budget means no directory was read");
}

/// Cancellation is the stronger fact: a caller who has given up is told that,
/// not that a clock it no longer cares about ran out.
#[test]
fn a_cancelled_walk_reports_cancellation_rather_than_the_deadline() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    scratch.file("root/report.txt");

    let cancel = CancelToken::new();
    cancel.cancel();
    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search.walk(
        &FileSearchQuery {
            normalized: "report".to_owned(),
            limit: 50,
            // Expired as well, so this pins which of the two reasons is
            // reported when both apply.
            deadline: Duration::ZERO,
            cancel,
        },
        Instant::now(),
    );

    assert_eq!(results.coverage, FileSearchCoverage::Cancelled);
    assert!(
        results.hits.is_empty(),
        "a search cancelled before it began read no directory"
    );
}

/// The walk is the one part of this backend that stops when it is asked to, and
/// the hits it already had are the point: a superseding keystroke usually shares
/// a prefix, so throwing them away costs the user work that was already done.
///
/// The fixture is two roots, and their order is what makes this deterministic
/// rather than a race. The walk is breadth first over `roots` in order, so every
/// hit lives in the first root and is found before the second is opened, while
/// the second root is a long stretch of work with no hit in it -- the stretch a
/// cancellation has to cut short. The bulk root's cost is name folding, not
/// filesystem traffic, so a warm page cache does not shrink the window the
/// canceller aims at.
#[test]
fn a_cancelled_walk_keeps_its_hits_and_stops_before_the_work_is_done() {
    /// Long enough that folding one name to lowercase and scanning it costs
    /// real time, which is what makes the bulk root's duration cpu bound.
    const PADDING: usize = 180;

    let scratch = Scratch::new();
    let matches = scratch.directory("matches");
    for index in 0..3 {
        scratch.file(&format!("matches/report-{index}.txt"));
    }
    let bulk = scratch.directory("bulk");
    let padding = "N".repeat(PADDING);
    for index in 0..20_000 {
        scratch.file(&format!("bulk/{padding}-{index:05}.bin"));
    }

    let search = WindowsFileSearch::with_roots(vec![matches, bulk]);

    // The margin is measured on this host rather than assumed. An uncancelled
    // walk over the same fixture says how long the work takes here, and the
    // cancellation is scheduled at a fraction of that, so the test does not
    // depend on how fast this machine is.
    let clock = Instant::now();
    let complete = search.walk(&query("report", 500), clock);
    let uncancelled = clock.elapsed();
    assert_eq!(complete.hits.len(), 3);
    assert_eq!(complete.coverage, FileSearchCoverage::Partial);
    assert!(
        uncancelled >= Duration::from_millis(8),
        "the fixture must take long enough to walk that there is something to interrupt, took \
         {uncancelled:?}"
    );

    let cancelled = query("report", 500);
    let token = cancelled.cancel.clone();
    let delay = uncancelled / 4;
    let canceller = thread::spawn(move || {
        thread::sleep(delay);
        token.cancel();
    });

    let started = Instant::now();
    let results = search.walk(&cancelled, started);
    let elapsed = started.elapsed();
    canceller.join().expect("the canceller thread does not panic");

    assert_eq!(results.coverage, FileSearchCoverage::Cancelled);
    assert_eq!(
        results.hits.len(),
        3,
        "the hits found before the cancellation are returned, not discarded"
    );
    assert!(
        elapsed < uncancelled,
        "a cancelled walk returns sooner than a complete one: {elapsed:?} against {uncancelled:?}"
    );
}

/// The service-level promise, not just the walk's: a cancelled search is an
/// answer with `Cancelled` coverage, never an error and never a full walk.
#[test]
fn a_cancelled_search_answers_with_cancelled_coverage_on_any_host() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    scratch.file("root/report.txt");

    let request = query("report", 50);
    request.cancel.cancel();
    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search
        .search(&request)
        .expect("a cancelled search is not a failure");

    assert_eq!(results.coverage, FileSearchCoverage::Cancelled);
    assert!(results.hits.is_empty());
}

/// A profile without a Videos folder is ordinary, and one unreadable directory
/// must not delete the hits every other root contributed.
#[test]
fn a_missing_root_does_not_hide_the_others() {
    let scratch = Scratch::new();
    let present = scratch.directory("present");
    scratch.file("present/report.txt");
    let absent = scratch.missing("absent");

    let search = WindowsFileSearch::with_roots(vec![absent, present]);
    let results = search.walk(&query("report", 50), Instant::now());

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].name, "report.txt");
}

#[test]
fn the_walk_descends_but_not_past_its_depth_cap() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    // One inside the cap, one past it.
    let mut shallow = String::from("root");
    for _ in 0..WindowsFileSearch::MAX_DEPTH {
        shallow.push_str("/deeper");
    }
    scratch.file(&format!("{shallow}/report-near.txt"));
    scratch.file(&format!("{shallow}/deeper/deeper/report-far.txt"));

    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search.walk(&query("report", 50), Instant::now());

    let names: Vec<&str> = results.hits.iter().map(|hit| hit.name.as_str()).collect();
    assert!(
        names.contains(&"report-near.txt"),
        "a real tree this deep is still searched: {names:?}"
    );
    assert!(
        !names.contains(&"report-far.txt"),
        "past the cap the walk stops, so a junction cycle cannot run forever: {names:?}"
    );
}

/// A reparse point or symlink is a hit when its name matches but never a
/// directory to descend into, because a link back up the tree would otherwise
/// make the walk run until the deadline every time.
#[cfg(unix)]
#[test]
fn a_link_is_reported_without_being_followed() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    scratch.file("root/inner/report-inside.txt");
    std::os::unix::fs::symlink(&root, root.join("report-loop")).expect("fixture symlink is creatable");

    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search.walk(&query("report", 50), Instant::now());

    let names: Vec<&str> = results.hits.iter().map(|hit| hit.name.as_str()).collect();
    assert!(names.contains(&"report-loop"), "the link itself matches");
    assert_eq!(
        names.iter().filter(|name| **name == "report-inside.txt").count(),
        1,
        "the target is reached once, through the real tree, not again through \
         the link: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// The service

#[test]
fn a_searcher_told_where_to_look_answers_from_there_on_any_host() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    scratch.file("root/report.txt");

    let search = WindowsFileSearch::with_roots(vec![root]);
    assert_eq!(
        search.source_name(),
        WindowsFileSearch::UNTRIED_SOURCE,
        "before the first search nothing has answered, so nothing is named"
    );
    assert!(
        !search.uses_index(),
        "a searcher given explicit roots never consults the catalog"
    );

    let results = search
        .search(&query("report", 50))
        .expect("a searcher with a readable root can answer");

    assert_eq!(results.hits.len(), 1);
    assert_eq!(
        search.source_name(),
        WindowsFileSearch::WALK_SOURCE,
        "the walk answered, and the diagnostic has to say which mechanism did"
    );
}

/// No name contains the empty string in a way worth reporting, and nothing was
/// left unsearched, so this is a complete answer rather than a truncated one.
#[test]
fn an_empty_query_is_answered_completely_and_emptily() {
    let scratch = Scratch::new();
    let root = scratch.directory("root");
    scratch.file("root/report.txt");

    let search = WindowsFileSearch::with_roots(vec![root]);
    let results = search
        .search(&query("", 50))
        .expect("an empty query is not a failure");

    assert!(results.hits.is_empty());
    assert_eq!(results.coverage, FileSearchCoverage::Complete);
}

/// The honesty rule this crate is built around: a backend that cannot reach
/// Windows says so rather than reporting an empty result set that reads as "you
/// have no such file".
#[cfg(not(target_os = "windows"))]
#[test]
fn off_target_a_session_searcher_refuses_instead_of_answering_emptily() {
    let search = WindowsFileSearch::new();
    assert!(
        search.roots().is_empty(),
        "there are no Windows profile folders to walk here"
    );

    match search.search(&query("report", 50)) {
        Err(CoreError::Invalid(reason)) => assert!(
            reason.contains("does not target Windows"),
            "the refusal should say why: {reason}"
        ),
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

/// Downloads is the folder this walk exists for: Windows Search in its default
/// Classic mode indexes Documents, Pictures, Music and the Desktop, and a
/// launcher is asked about a just-downloaded file constantly.
#[test]
fn the_walk_covers_the_profile_folders_the_default_index_does_not() {
    assert!(WALK_SUBDIRECTORIES.contains(&"Downloads"));
    assert!(WALK_SUBDIRECTORIES.contains(&"Desktop"));
    assert!(
        !WALK_SUBDIRECTORIES.contains(&"AppData"),
        "offering the user twenty thousand cache files is worse than offering none"
    );
}
