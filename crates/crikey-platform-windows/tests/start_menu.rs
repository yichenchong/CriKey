//! Public-API contract for Windows application discovery (spec 10.2, 18.4).
//!
//! This is the discovery half of the M1 "global hotkey + app discovery"
//! deliverable on Windows. Resolving a `.lnk` needs COM and is therefore not
//! pinned here; everything around it is, because everything around it is where
//! a wrong answer would be silent: which files a scan finds and in what order,
//! how a shortcut's argument string becomes an argument vector, and which of
//! two discoveries pointing at one program survives.
//!
//! The deduplication rule is load bearing rather than cosmetic.
//! `crikey_platform::application_items` derives an item's stable id from the
//! encoded target alone, so two discoveries sharing a target are one catalog
//! item whatever this crate does; collapsing them here is what makes the
//! choice deterministic instead of leaving the last writer to win.
//!
//! Every case writes real files into a unique temp directory that is removed
//! when the test ends, so runs are order independent and leave nothing behind.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(not(target_os = "windows"))]
use crikey_core::CoreError;
use crikey_core::PlatformPath;
#[cfg(not(target_os = "windows"))]
use crikey_platform::ApplicationDiscovery;
use crikey_platform::DiscoveredApplication;
use crikey_platform_windows::{split_arguments, ApplicationSet, StartMenuDiscovery};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

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
            "crikey-start-menu-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak fixtures into this one.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    /// An existing Start Menu root inside the scratch directory.
    fn root(&self, name: &str) -> PathBuf {
        let root = self.path.join(name);
        fs::create_dir_all(&root).expect("start menu root is creatable");
        root
    }

    /// A scratch path that is deliberately never created.
    fn missing(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes a placeholder shortcut. Contents are never parsed by the walk: the
/// shell link object reads the file, and it only ever sees paths this returns.
fn write_shortcut(root: &Path, relative: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("shortcut folder is creatable");
    }
    fs::write(&path, b"L\0\0\0").expect("shortcut is writable");
    path
}

fn scanner(roots: Vec<PathBuf>) -> StartMenuDiscovery {
    StartMenuDiscovery::with_roots(roots, false)
}

fn names(scanner: &StartMenuDiscovery) -> Vec<String> {
    scanner
        .shortcuts()
        .into_iter()
        .map(|shortcut| shortcut.name)
        .collect()
}

fn application(name: &str, target: &str) -> DiscoveredApplication {
    DiscoveredApplication {
        name: name.to_owned(),
        target: PlatformPath::new(target),
        arguments: Vec::new(),
        working_directory: None,
        icon_reference: None,
        platform_id: None,
    }
}

// ---------------------------------------------------------------------------
// The Start Menu walk
// ---------------------------------------------------------------------------

#[test]
fn shortcuts_are_found_below_the_root() {
    let scratch = Scratch::new();
    let root = scratch.root("start-menu");
    write_shortcut(&root, "Programs/Firefox.lnk");
    write_shortcut(&root, "Programs/Accessories/Notepad.lnk");

    let found = names(&scanner(vec![root]));
    assert_eq!(found, vec!["Notepad", "Firefox"]);
}

#[test]
fn the_walk_is_ordered_and_repeatable() {
    // Filesystem order is not an order. A rescan of an unchanged menu must
    // produce the same list, because root precedence -- and therefore which of
    // two duplicates survives -- is defined in terms of it.
    let scratch = Scratch::new();
    let root = scratch.root("start-menu");
    for relative in [
        "Zed.lnk",
        "Alpha.lnk",
        "Middle.lnk",
        "Games/Solitaire.lnk",
        "Accessories/Calculator.lnk",
    ] {
        write_shortcut(&root, relative);
    }

    let scanner = scanner(vec![root]);
    let first = names(&scanner);
    let second = names(&scanner);

    assert_eq!(first, second);
    // Names sort together, folders and shortcuts alike, and a folder is
    // descended into where its name falls: Accessories, Alpha.lnk, Games,
    // Middle.lnk, Zed.lnk.
    assert_eq!(first, vec!["Calculator", "Alpha", "Solitaire", "Middle", "Zed"]);
}

#[test]
fn roots_are_walked_in_the_order_they_were_given() {
    let scratch = Scratch::new();
    let user = scratch.root("user");
    let machine = scratch.root("machine");
    write_shortcut(&user, "Editor.lnk");
    write_shortcut(&machine, "Editor.lnk");

    let found = scanner(vec![user.clone(), machine.clone()]).shortcuts();
    assert_eq!(found.len(), 2);
    assert!(found[0].path.starts_with(&user));
    assert!(found[1].path.starts_with(&machine));
}

#[test]
fn only_shortcut_files_are_offered_to_the_shell() {
    let scratch = Scratch::new();
    let root = scratch.root("start-menu");
    write_shortcut(&root, "Real.lnk");
    write_shortcut(&root, "Upper.LNK");
    write_shortcut(&root, "Backup.lnk.bak");
    write_shortcut(&root, "lnk");
    write_shortcut(&root, ".lnk");
    write_shortcut(&root, "Readme.txt");
    fs::create_dir_all(root.join("Folder.lnk")).expect("directory is creatable");

    let mut found = names(&scanner(vec![root]));
    found.sort();
    assert_eq!(found, vec!["Real", "Upper"]);
}

#[cfg(unix)]
#[test]
fn a_shortcut_with_non_utf8_name_is_still_found() {
    let scratch = Scratch::new();
    let root = scratch.root("start-menu");
    let name = std::ffi::OsString::from_vec(b"legacy-\xFF.LNK".to_vec());
    fs::write(root.join(name), b"L\0\0\0").expect("shortcut is writable");

    assert_eq!(names(&scanner(vec![root])), vec!["legacy-\u{FFFD}"]);
}

#[test]
fn a_missing_or_unreadable_root_is_skipped_not_reported() {
    // A machine with no per-user Start Menu is ordinary, and it must not hide
    // the machine-wide one.
    let scratch = Scratch::new();
    let present = scratch.root("machine");
    write_shortcut(&present, "Editor.lnk");

    let found = names(&scanner(vec![scratch.missing("nowhere"), present]));
    assert_eq!(found, vec!["Editor"]);
}

#[test]
fn nesting_past_the_depth_cap_is_not_followed() {
    // The cap exists for a directory junction pointing at an ancestor, which
    // would otherwise be an unbounded walk.
    let scratch = Scratch::new();
    let root = scratch.root("start-menu");

    let mut relative = String::new();
    for level in 0..=StartMenuDiscovery::MAX_DEPTH + 2 {
        write_shortcut(&root, &format!("{relative}Level{level}.lnk"));
        relative.push_str(&format!("Level{level}/"));
    }

    let found = names(&scanner(vec![root]));
    // Level0 sits at depth 0, so the deepest reachable entry is at MAX_DEPTH.
    assert_eq!(found.len(), StartMenuDiscovery::MAX_DEPTH + 1);
    assert!(found.contains(&format!("Level{}", StartMenuDiscovery::MAX_DEPTH)));
    assert!(!found.contains(&format!("Level{}", StartMenuDiscovery::MAX_DEPTH + 1)));
}

#[test]
fn a_shortcut_is_named_after_itself_not_its_extension() {
    let scratch = Scratch::new();
    let root = scratch.root("start-menu");
    write_shortcut(&root, "Visual Studio Code.lnk");

    let found = scanner(vec![root]).shortcuts();
    assert_eq!(found[0].name, "Visual Studio Code");
    assert_eq!(
        found[0].path.file_name().and_then(|name| name.to_str()),
        Some("Visual Studio Code.lnk")
    );
}

#[test]
fn a_scanner_reports_the_roots_it_was_given() {
    let scratch = Scratch::new();
    let roots = vec![scratch.root("user"), scratch.root("machine")];
    let scanner = StartMenuDiscovery::with_roots(roots.clone(), true);

    assert_eq!(scanner.roots(), roots.as_slice());
    assert!(scanner.packaged());
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

#[test]
fn the_first_discovery_of_a_target_wins() {
    let mut set = ApplicationSet::new();
    assert!(set.insert(application("Firefox", r"C:\Program Files\Firefox\firefox.exe")));
    assert!(!set.insert(application(
        "Mozilla Firefox",
        r"C:\Program Files\Firefox\firefox.exe"
    )));

    let applications = set.into_applications();
    assert_eq!(applications.len(), 1);
    assert_eq!(applications[0].name, "Firefox");
}

#[test]
fn targets_are_compared_the_way_windows_compares_paths() {
    // One file, two spellings. A launcher that listed both would be listing
    // the same program twice, and both would collapse onto one catalog item
    // anyway once the item id is derived.
    let mut set = ApplicationSet::new();
    assert!(set.insert(application("Notepad", r"C:\Windows\System32\notepad.exe")));
    assert!(!set.insert(application("notepad", r"c:\windows\system32\NOTEPAD.EXE")));
    assert_eq!(set.len(), 1);
}

#[cfg(unix)]
#[test]
fn targets_with_different_non_utf8_units_are_not_deduplicated() {
    let mut first = application("First", "placeholder");
    first.target = PlatformPath::new(std::ffi::OsString::from_vec(b"C:\\app-\xFF.exe".to_vec()));
    let mut second = application("Second", "placeholder");
    second.target = PlatformPath::new(std::ffi::OsString::from_vec(b"C:\\app-\xFE.exe".to_vec()));

    let mut set = ApplicationSet::new();
    assert!(set.insert(first));
    assert!(set.insert(second));
    assert_eq!(set.len(), 2);
}
#[test]
fn different_targets_are_different_applications() {
    let mut set = ApplicationSet::new();
    assert!(set.insert(application("Notepad", r"C:\Windows\System32\notepad.exe")));
    assert!(set.insert(application(
        "Notepad++",
        r"C:\Program Files\Notepad++\notepad++.exe"
    )));
    assert!(set.insert(application(
        "Calculator",
        r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"
    )));
    assert_eq!(set.len(), 3);
}

#[test]
fn a_kept_target_is_not_folded_into_its_dedup_key() {
    // The key is case folded and lossy; the target is neither. Losing the
    // original spelling would hand the launcher a path it never saw
    // (spec 18.3, ADR-0007).
    let original = r"C:\Program Files\JetBrains\CLion 2024.1\bin\clion64.exe";
    let mut set = ApplicationSet::new();
    set.insert(application("CLion", original));

    let applications = set.into_applications();
    assert_eq!(applications[0].target, PlatformPath::new(original));
}

#[test]
fn an_application_with_no_target_names_nothing_to_launch() {
    let mut set = ApplicationSet::new();
    assert!(!set.insert(application("Ghost", "")));
    assert!(set.is_empty());

    // And the refusal must not have claimed the empty key for a later entry.
    assert!(set.insert(application("Real", r"C:\real.exe")));
    assert_eq!(set.len(), 1);
}

#[test]
fn insertion_order_is_the_reported_order() {
    let mut set = ApplicationSet::new();
    for (name, target) in [
        ("Alpha", r"C:\alpha.exe"),
        ("Beta", r"C:\beta.exe"),
        ("Gamma", r"C:\gamma.exe"),
    ] {
        set.insert(application(name, target));
    }

    let reported: Vec<String> = set
        .into_applications()
        .into_iter()
        .map(|application| application.name)
        .collect();
    assert_eq!(reported, vec!["Alpha", "Beta", "Gamma"]);
}

// ---------------------------------------------------------------------------
// Shortcut arguments
// ---------------------------------------------------------------------------

#[test]
fn an_empty_argument_string_is_no_arguments() {
    assert!(split_arguments("").is_empty());
    assert!(split_arguments("   \t ").is_empty());
}

#[test]
fn arguments_split_on_unquoted_whitespace() {
    assert_eq!(split_arguments("--new-window"), vec!["--new-window"]);
    assert_eq!(
        split_arguments("--profile default --new-window"),
        vec!["--profile", "default", "--new-window"]
    );
    assert_eq!(split_arguments("  a\tb  c "), vec!["a", "b", "c"]);
}

#[test]
fn quotes_hold_a_path_with_spaces_together() {
    // The reason this splitter exists: re-splitting on spaces later would turn
    // one Program Files path into three arguments.
    assert_eq!(
        split_arguments(r#"/open "C:\Program Files\App\config.ini""#),
        vec!["/open", r"C:\Program Files\App\config.ini"]
    );
}

#[test]
fn a_quoted_run_may_be_part_of_an_argument() {
    assert_eq!(
        split_arguments(r#"--path="C:\Program Files\App""#),
        vec![r"--path=C:\Program Files\App"]
    );
}

#[test]
fn an_explicitly_empty_argument_survives() {
    // `""` is an argument the author asked for; `  ` is not an argument at all.
    assert_eq!(split_arguments(r#"a "" b"#), vec!["a", "", "b"]);
    assert_eq!(split_arguments(r#""""#), vec![""]);
}

#[test]
fn backslashes_are_literal_unless_they_precede_a_quote() {
    assert_eq!(split_arguments(r"C:\path\to\file"), vec![r"C:\path\to\file"]);
    assert_eq!(split_arguments(r"a\\\\b"), vec![r"a\\\\b"]);
    // Trailing backslashes belong to the argument they end.
    assert_eq!(split_arguments(r"C:\dir\ next"), vec![r"C:\dir\", "next"]);
}

#[test]
fn a_backslash_escapes_the_quote_that_follows_it() {
    // One backslash and a quote: a literal quote, not a quoted run.
    assert_eq!(split_arguments(r#"\"quoted\""#), vec![r#""quoted""#]);
    // Two backslashes and a quote: one backslash, and the quote opens a run.
    assert_eq!(split_arguments(r#"\\"a b""#), vec![r"\a b"]);
    // Three: one backslash and a literal quote.
    assert_eq!(split_arguments(r#"\\\"a"#), vec![r#"\"a"#]);
}

#[test]
fn a_doubled_quote_inside_a_quoted_run_is_one_quote() {
    assert_eq!(split_arguments(r#""a""b""#), vec![r#"a"b"#]);
}

#[test]
fn an_unterminated_quote_yields_the_rest_of_the_string() {
    // Malformed, but a shortcut in the wild is not a reason to lose the entry.
    assert_eq!(
        split_arguments(r#"--open "C:\Program Files\App"#),
        vec!["--open", r"C:\Program Files\App"]
    );
}

// ---------------------------------------------------------------------------
// Off-target honesty
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
#[test]
fn off_target_discovery_refuses_instead_of_reporting_nothing() {
    // An empty list would read as "this machine has no applications", which is
    // a different and much worse answer than "this build cannot look".
    let scratch = Scratch::new();
    let root = scratch.root("start-menu");
    write_shortcut(&root, "Editor.lnk");

    match scanner(vec![root]).discover() {
        Err(CoreError::Invalid(reason)) => assert!(
            reason.contains("does not target Windows"),
            "the refusal should say why: {reason}"
        ),
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

#[cfg(not(target_os = "windows"))]
#[test]
fn off_target_there_are_no_known_folders_to_scan() {
    assert!(StartMenuDiscovery::new().roots().is_empty());
}
