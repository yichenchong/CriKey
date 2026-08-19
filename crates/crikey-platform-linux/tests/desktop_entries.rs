//! Public-API contract for XDG desktop-entry application discovery (spec 18.6).
//!
//! This is the discovery half of the M1 "global hotkey + app discovery"
//! deliverable on Linux (roadmap M1): a scanner over an ordered, caller-supplied
//! list of `applications` roots that yields `DiscoveredApplication` values, plus
//! honest capability reporting (spec 18.2). Targets stay `PlatformPath` so
//! non-UTF-8 install paths survive (spec 18.3).
//!
//! Only the parsing rules the core actually depends on are pinned here: group
//! scoping, `Type=Application`, visibility keys, `Exec` tokenization, and root
//! precedence. Locale selection, action groups as launchable entries, and
//! recursive root layouts are deliberately outside this contract.
//!
//! What a candidate file is allowed to be is pinned too: anything in a root
//! that is not an ordinary file of sane size -- a FIFO, a device, a directory,
//! an oversized file -- must be skipped without blocking the scan or reading
//! without bound.
//!
//! Every case writes real files into a unique temp directory that is removed
//! when the test ends, so runs are order independent and leave nothing behind.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crikey_platform::{ApplicationDiscovery, Capability, CapabilityState, DiscoveredApplication};
use crikey_platform_linux::{DesktopEntryScanner, DesktopEnvironment, LinuxBackend};

const FIREFOX: &str = "[Desktop Entry]
Type=Application
Name=Firefox Web Browser
Exec=/usr/bin/firefox %u
Icon=firefox
Comment=Browse the web
";

const CALCULATOR: &str = "[Desktop Entry]
Type=Application
Name=Calculator
Exec=/usr/bin/galculator
Icon=accessories-calculator
";

const WITHOUT_ICON: &str = "[Desktop Entry]
Type=Application
Name=Plain Tool
Exec=/usr/bin/plain-tool
";

const WITHOUT_EXEC: &str = "[Desktop Entry]
Type=Application
Name=Unlaunchable
Icon=missing-exec
";

const TYPE_LINK: &str = "[Desktop Entry]
Type=Link
Name=Bookmark
URL=https://example.invalid/
Exec=/usr/bin/bookmark
Icon=bookmark
";

const TYPE_DIRECTORY: &str = "[Desktop Entry]
Type=Directory
Name=Games Folder
Exec=/usr/bin/games-folder
Icon=folder
";

const WITHOUT_TYPE: &str = "[Desktop Entry]
Name=Untyped
Exec=/usr/bin/untyped
Icon=untyped
";

const NO_DISPLAY: &str = "[Desktop Entry]
Type=Application
Name=Background Helper
Exec=/usr/libexec/background-helper
NoDisplay=true
";

const HIDDEN: &str = "[Desktop Entry]
Type=Application
Name=Uninstalled Leftover
Exec=/usr/bin/leftover
Hidden=true
";
const TERMINAL_ENTRY: &str = "[Desktop Entry]
Type=Application
Name=Needs Terminal
Exec=/usr/bin/needs-terminal
Terminal=true
";

const EXPLICITLY_VISIBLE: &str = "[Desktop Entry]
Type=Application
Name=Visible Tool
Exec=/usr/bin/visible-tool
NoDisplay=false
Hidden=false
Icon=visible-tool
";

/// Every field code the contract strips, surrounded by arguments that must live.
const FIELD_CODES: &str = "[Desktop Entry]
Type=Application
Name=Image Editor
Exec=/usr/bin/gimp -n %F --no-splash %f %u %U %i %c %k --verbose
Icon=gimp
";

const QUOTED_EXEC: &str = "[Desktop Entry]
Type=Application
Name=Notes
Exec=\"/opt/My Apps/notes\" --title \"Daily Notes\" --tag work %U
Icon=notes
";

/// Field codes inside double quotes are launcher substitutions just the same,
/// and `%%` is the only way to write a literal percent anywhere in an `Exec`.
const QUOTED_FIELD_CODES: &str = "[Desktop Entry]
Type=Application
Name=Viewer
Exec=/usr/bin/viewer --file \"%f\" --scale 100%% --label \"50%% done\" \"keep me\" %U
Icon=viewer
";

/// Every string escape the format defines, plus an undefined one that has to
/// survive exactly as the author wrote it.
const ESCAPED_STRINGS: &str = "[Desktop Entry]
Type=Application
Name=Sound\\sand\\sVideo\\\\Tools\\nSecond\\tTabbed\\rReturned\\qUnknown
Exec=/usr/bin/media-tools
Icon=media\\splayer
";

/// An action group repeats `Name`, `Exec` and `Icon`; a flat key scan would
/// silently launch the action instead of the application.
const WITH_ACTION_GROUP: &str = "[Desktop Entry]
Type=Application
Name=Terminal
Exec=/usr/bin/xterm
Icon=utilities-terminal
Actions=NewWindow;

[Desktop Action NewWindow]
Name=New Window
Exec=/usr/bin/xterm -e new-window
Icon=window-new
";

/// Keys ahead of `[Desktop Entry]` belong to another group and must not
/// disqualify or rewrite the entry.
const WITH_LEADING_GROUP: &str = "[X-Custom Preamble]
Type=Link
Name=Preamble
Exec=/usr/bin/preamble
NoDisplay=true
Hidden=true

[Desktop Entry]
Type=Application
Name=Archive Manager
Exec=/usr/bin/file-roller %U
Icon=file-roller
";

/// Comments, blank lines, separator-less lines, empty keys, extension keys and
/// unknown-locale variants all appear in the wild and must not abort the parse.
const MESSY_BUT_VALID: &str = "[Desktop Entry]

# A comment about this entry
Type=Application
Name=Files
Name[xx]=Bogus Locale Variant
X-GNOME-FullName=Files Deluxe
this line has no separator at all
=value without a key
Exec=/usr/bin/nautilus --new-window %U
Terminal=false
Categories=System;FileTools;
UnknownKeyFromTheFuture=ignored
Icon=system-file-manager
";

const EDITOR_FROM_USER: &str = "[Desktop Entry]
Type=Application
Name=Editor (user)
Exec=/home/tester/bin/editor
Icon=editor-user
";

const EDITOR_FROM_SYSTEM: &str = "[Desktop Entry]
Type=Application
Name=Editor (system)
Exec=/usr/bin/editor
Icon=editor-system
";

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
            "crikey-desktop-entries-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak fixtures into this one.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    /// An existing `applications` root inside the scratch directory.
    fn root(&self, name: &str) -> PathBuf {
        let root = self.path.join(name);
        fs::create_dir_all(&root).expect("applications root is creatable");
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

fn write_entry(root: &Path, file_name: &str, contents: &str) {
    fs::write(root.join(file_name), contents).expect("fixture entry is writable");
}

/// A FIFO named like an entry: opening one blocks until a writer shows up, so
/// a scanner that opens before it stats hangs on a file any user can drop into
/// `~/.local/share/applications`.
fn make_fifo(root: &Path, file_name: &str) {
    let status = Command::new("mkfifo")
        .arg(root.join(file_name))
        .status()
        .expect("mkfifo is available on a Linux host");

    assert!(status.success(), "mkfifo could not create {file_name}");
}

/// A valid entry padded with a comment line to exactly `size` bytes.
fn padded_entry(name: &str, size: usize) -> String {
    let head = format!("[Desktop Entry]\nType=Application\nName={name}\nExec=/usr/bin/padded\n#");
    // Padding is a comment, so size is the only thing that varies between the
    // entry that fits the cap and the one that does not.
    let padding = size
        .checked_sub(head.len().saturating_add(1))
        .expect("the requested size has room for the entry itself");

    format!("{head}{}\n", "p".repeat(padding))
}

/// Scans on a worker thread so a regression that blocks on a candidate fails
/// the test instead of hanging the run.
///
/// The bound is a liveness guard, never a timing assertion: a correct scan of
/// a three-file root returns in microseconds and the wait ends with it.
fn discover_without_blocking(roots: Vec<PathBuf>) -> Vec<DiscoveredApplication> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        // A failed send only means this test already gave up.
        let _ = sender.send(discover(roots));
    });

    receiver
        .recv_timeout(Duration::from_secs(60))
        .expect("discovery must return instead of blocking on a candidate entry")
}

/// Scans through `&dyn ApplicationDiscovery` so the backend trait object the
/// app wires in (spec 18.1) stays usable.
fn discover(roots: Vec<PathBuf>) -> Vec<DiscoveredApplication> {
    let scanner = DesktopEntryScanner::new(roots);
    let discovery: &dyn ApplicationDiscovery = &scanner;

    discovery.discover().expect("scanning well-formed roots succeeds")
}

/// Discovery order is unspecified, so every assertion compares sorted names.
fn names(applications: &[DiscoveredApplication]) -> Vec<&str> {
    let mut names: Vec<&str> = applications
        .iter()
        .map(|application| application.name.as_str())
        .collect();
    names.sort_unstable();
    names
}

fn only(applications: &[DiscoveredApplication]) -> &DiscoveredApplication {
    match applications {
        [single] => single,
        other => panic!("expected exactly one application, discovered {:?}", names(other)),
    }
}

fn by_name<'a>(applications: &'a [DiscoveredApplication], name: &str) -> &'a DiscoveredApplication {
    applications
        .iter()
        .find(|application| application.name == name)
        .unwrap_or_else(|| panic!("{name:?} missing from {:?}", names(applications)))
}

fn target(application: &DiscoveredApplication) -> &str {
    application
        .target
        .as_path()
        .to_str()
        .expect("fixture targets are utf-8")
}

#[test]
fn a_visible_entry_is_discovered_with_its_name_exec_and_icon() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "firefox.desktop", FIREFOX);

    let applications = discover(vec![root.clone()]);
    let firefox = only(&applications);

    assert_eq!(firefox.name, "Firefox Web Browser");
    assert_eq!(target(firefox), "/usr/bin/firefox");
    assert!(
        firefox.arguments.is_empty(),
        "%u is a field code, not an argument: {:?}",
        firefox.arguments
    );
    assert_eq!(firefox.icon_reference.as_deref(), Some("firefox"));
    assert_eq!(firefox.working_directory.as_ref(), None);

    let platform_id = firefox
        .platform_id
        .as_deref()
        .expect("a desktop entry carries its native desktop id");
    assert!(
        platform_id.contains("firefox"),
        "the desktop id must identify the entry file, got {platform_id:?}"
    );

    // Discovery is a pure read: rescanning an unchanged root repeats itself.
    assert_eq!(names(&discover(vec![root])), ["Firefox Web Browser"]);
}

#[test]
fn a_desktop_entry_records_its_working_directory() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(
        &root,
        "editor.desktop",
        "[Desktop Entry]\nType=Application\nName=Editor\nPath=/home/tester/project\nExec=/usr/bin/editor\n",
    );

    let applications = discover(vec![root]);
    let editor = only(&applications);
    assert_eq!(
        editor
            .working_directory
            .as_ref()
            .map(|path| path.display().to_string()),
        Some("/home/tester/project".to_owned())
    );
}

#[test]
fn an_entry_without_an_icon_key_discovers_without_an_icon_reference() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "plain-tool.desktop", WITHOUT_ICON);

    let applications = discover(vec![root]);
    let plain = only(&applications);

    assert_eq!(plain.name, "Plain Tool");
    assert_eq!(target(plain), "/usr/bin/plain-tool");
    assert_eq!(plain.icon_reference, None);
}

#[test]
fn only_entries_typed_as_application_are_discovered() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "firefox.desktop", FIREFOX);
    write_entry(&root, "bookmark.desktop", TYPE_LINK);
    write_entry(&root, "games.desktop", TYPE_DIRECTORY);
    write_entry(&root, "untyped.desktop", WITHOUT_TYPE);

    assert_eq!(names(&discover(vec![root])), ["Firefox Web Browser"]);
}

#[test]
fn an_entry_without_an_exec_key_is_skipped() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "unlaunchable.desktop", WITHOUT_EXEC);
    write_entry(&root, "calculator.desktop", CALCULATOR);

    assert_eq!(names(&discover(vec![root])), ["Calculator"]);
}

#[test]
fn nodisplay_and_hidden_entries_are_skipped_while_explicit_false_is_kept() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "background-helper.desktop", NO_DISPLAY);
    write_entry(&root, "leftover.desktop", HIDDEN);
    write_entry(&root, "visible-tool.desktop", EXPLICITLY_VISIBLE);

    let applications = discover(vec![root]);

    assert_eq!(names(&applications), ["Visible Tool"]);
    assert_eq!(target(only(&applications)), "/usr/bin/visible-tool");
}

#[test]
fn terminal_entries_are_not_presented_as_detached_launches() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "needs-terminal.desktop", TERMINAL_ENTRY);
    write_entry(&root, "calculator.desktop", CALCULATOR);

    assert_eq!(names(&discover(vec![root])), ["Calculator"]);
}

#[test]
fn exec_field_codes_are_stripped_while_real_arguments_survive() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "gimp.desktop", FIELD_CODES);

    let applications = discover(vec![root]);
    let editor = only(&applications);

    assert_eq!(target(editor), "/usr/bin/gimp");
    assert_eq!(editor.arguments, ["-n", "--no-splash", "--verbose"]);
}

#[test]
fn quoted_exec_tokens_stay_single_arguments() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "notes.desktop", QUOTED_EXEC);

    let applications = discover(vec![root]);
    let notes = only(&applications);

    assert_eq!(target(notes), "/opt/My Apps/notes");
    assert_eq!(notes.arguments, ["--title", "Daily Notes", "--tag", "work"]);
}

#[test]
fn exec_escapes_preserve_literal_backslashes_and_quotes() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(
        &root,
        "escaped.desktop",
        r#"[Desktop Entry]
Type=Application
Name=Escaped arguments
Exec="/usr/bin/escape-fixture" "C:\\\\Program Files" "say\"hello"
"#,
    );

    let applications = discover(vec![root]);
    let application = only(&applications);
    assert_eq!(target(application), "/usr/bin/escape-fixture");
    assert_eq!(application.arguments, ["C:\\Program Files", "say\"hello"]);
}

#[test]
fn unknown_field_codes_and_unterminated_quotes_are_skipped() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(
        &root,
        "unknown-code.desktop",
        "[Desktop Entry]\nType=Application\nName=Unknown code\nExec=/bin/false %x\n",
    );
    write_entry(
        &root,
        "unterminated.desktop",
        "[Desktop Entry]\nType=Application\nName=Unterminated\nExec=/bin/false \"arg\n",
    );
    write_entry(&root, "calculator.desktop", CALCULATOR);
    write_entry(
        &root,
        "single-quote.desktop",
        "[Desktop Entry]\nType=Application\nName=Single quote\nExec=/bin/false 'arg with spaces'\n",
    );

    assert_eq!(names(&discover(vec![root])), ["Calculator"]);
}

/// The desktop-entry specification forbids field codes inside quoted
/// arguments, so their expansion is unspecified for malformed files. The
/// parser deliberately fails closed by stripping them rather than handing an
/// unexpanded placeholder to the launched program.
#[test]
fn field_codes_are_stripped_inside_quotes_and_double_percent_collapses() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "viewer.desktop", QUOTED_FIELD_CODES);

    let applications = discover(vec![root]);
    let viewer = only(&applications);

    assert_eq!(target(viewer), "/usr/bin/viewer");
    assert_eq!(
        viewer.arguments,
        ["--file", "--scale", "100%", "--label", "50% done", "keep me"],
        "a quoted field code is still a substitution and must not reach argv"
    );
}

#[test]
fn string_escapes_in_display_values_are_decoded() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "media-tools.desktop", ESCAPED_STRINGS);

    let applications = discover(vec![root]);
    let media = only(&applications);

    assert_eq!(
        media.name,
        "Sound and Video\\Tools\nSecond\tTabbed\rReturned\\qUnknown"
    );
    assert_eq!(media.icon_reference.as_deref(), Some("media player"));
    assert_eq!(target(media), "/usr/bin/media-tools");
}

#[test]
fn keys_outside_the_desktop_entry_group_are_ignored() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "xterm.desktop", WITH_ACTION_GROUP);
    write_entry(&root, "file-roller.desktop", WITH_LEADING_GROUP);

    let applications = discover(vec![root]);

    assert_eq!(names(&applications), ["Archive Manager", "Terminal"]);

    let terminal = by_name(&applications, "Terminal");
    assert_eq!(target(terminal), "/usr/bin/xterm");
    assert!(
        terminal.arguments.is_empty(),
        "the action group's arguments leaked: {:?}",
        terminal.arguments
    );
    assert_eq!(terminal.icon_reference.as_deref(), Some("utilities-terminal"));

    let archive_manager = by_name(&applications, "Archive Manager");
    assert_eq!(target(archive_manager), "/usr/bin/file-roller");
    assert_eq!(archive_manager.icon_reference.as_deref(), Some("file-roller"));
}

#[test]
fn malformed_lines_and_unknown_keys_are_tolerated() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "nautilus.desktop", MESSY_BUT_VALID);
    write_entry(&root, "calculator.desktop", CALCULATOR);

    let applications = discover(vec![root]);

    assert_eq!(names(&applications), ["Calculator", "Files"]);

    let files = by_name(&applications, "Files");
    assert_eq!(target(files), "/usr/bin/nautilus");
    assert_eq!(files.arguments, ["--new-window"]);
    assert_eq!(files.icon_reference.as_deref(), Some("system-file-manager"));
}

#[test]
fn a_duplicate_desktop_id_resolves_to_the_earliest_root() {
    let scratch = Scratch::new();
    let user = scratch.root("user");
    let system = scratch.root("system");
    write_entry(&user, "editor.desktop", EDITOR_FROM_USER);
    write_entry(&system, "editor.desktop", EDITOR_FROM_SYSTEM);
    write_entry(&system, "calculator.desktop", CALCULATOR);

    let user_first = discover(vec![user.clone(), system.clone()]);

    assert_eq!(names(&user_first), ["Calculator", "Editor (user)"]);
    assert_eq!(
        target(by_name(&user_first, "Editor (user)")),
        "/home/tester/bin/editor"
    );

    // Precedence follows root order, not entry content or filesystem order.
    let system_first = discover(vec![system, user]);

    assert_eq!(names(&system_first), ["Calculator", "Editor (system)"]);
    assert_eq!(
        target(by_name(&system_first, "Editor (system)")),
        "/usr/bin/editor"
    );
}

#[test]
fn missing_and_empty_roots_are_skipped_instead_of_failing_the_scan() {
    let scratch = Scratch::new();
    let present = scratch.root("applications");
    write_entry(&present, "firefox.desktop", FIREFOX);

    let around_gaps = discover(vec![
        scratch.missing("absent-before"),
        present,
        scratch.missing("absent-after"),
    ]);

    assert_eq!(names(&around_gaps), ["Firefox Web Browser"]);
    assert!(discover(vec![scratch.missing("nowhere")]).is_empty());
    assert!(discover(vec![scratch.root("empty")]).is_empty());
    assert!(discover(Vec::new()).is_empty());
}

#[test]
fn files_without_a_desktop_extension_are_ignored() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "firefox.desktop", FIREFOX);
    write_entry(&root, "calculator.txt", CALCULATOR);
    write_entry(&root, "calculator.desktop.bak", CALCULATOR);
    write_entry(&root, "desktop", CALCULATOR);
    write_entry(&root, "calculator.desktop.d", CALCULATOR);

    assert_eq!(names(&discover(vec![root])), ["Firefox Web Browser"]);
}

#[test]
fn a_fifo_or_device_named_like_an_entry_is_skipped_without_blocking_the_scan() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(&root, "firefox.desktop", FIREFOX);
    make_fifo(&root, "pipe.desktop");
    symlink("/dev/zero", root.join("zero.desktop")).expect("device symlink fixture is creatable");

    let applications = discover_without_blocking(vec![root]);

    assert_eq!(names(&applications), ["Firefox Web Browser"]);
}

#[test]
fn an_entry_past_the_maximum_size_is_skipped_while_one_at_the_maximum_is_read() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    let maximum = usize::try_from(DesktopEntryScanner::MAX_ENTRY_BYTES).expect("the cap fits a usize");

    write_entry(&root, "at-the-cap.desktop", &padded_entry("At The Cap", maximum));
    write_entry(
        &root,
        "past-the-cap.desktop",
        &padded_entry("Past The Cap", maximum.saturating_add(1)),
    );
    write_entry(&root, "firefox.desktop", FIREFOX);

    let applications = discover_without_blocking(vec![root]);

    assert_eq!(names(&applications), ["At The Cap", "Firefox Web Browser"]);
    assert_eq!(target(by_name(&applications, "At The Cap")), "/usr/bin/padded");
}

#[test]
fn a_directory_named_like_an_entry_is_ignored_and_leaves_the_id_unclaimed() {
    let scratch = Scratch::new();
    let user = scratch.root("user");
    let system = scratch.root("system");
    fs::create_dir(user.join("editor.desktop")).expect("directory fixture is creatable");
    write_entry(&user, "calculator.desktop", CALCULATOR);
    write_entry(&system, "editor.desktop", EDITOR_FROM_SYSTEM);

    let applications = discover(vec![user, system]);
    // A directory is not an entry, so it must not shadow the real one below it.
    assert_eq!(names(&applications), ["Calculator", "Editor (system)"]);
}

#[test]
fn a_non_utf8_entry_is_skipped_without_aborting_other_discovery() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    fs::write(
        root.join("broken.desktop"),
        b"[Desktop Entry]\nType=Application\nName=Broken \xFF\nExec=/bin/true\n",
    )
    .expect("non-UTF-8 fixture is writable");
    write_entry(&root, "working.desktop", CALCULATOR);

    let applications = discover(vec![root]);

    assert_eq!(names(&applications), ["Calculator"]);
}

#[test]
fn try_exec_requires_an_executable_file() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    let installed = scratch.path.join("installed-helper");
    fs::write(&installed, b"#!/bin/sh\n").expect("TryExec fixture is writable");
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755))
        .expect("TryExec fixture is executable");

    write_entry(
        &root,
        "installed.desktop",
        &format!(
            "[Desktop Entry]\nType=Application\nName=Installed\nTryExec={}\nExec=/bin/true\n",
            installed.display()
        ),
    );
    write_entry(
        &root,
        "missing.desktop",
        &format!(
            "[Desktop Entry]\nType=Application\nName=Missing\nTryExec={}\nExec=/bin/true\n",
            scratch.path.join("missing-helper").display()
        ),
    );

    let applications = discover(vec![root]);

    assert_eq!(names(&applications), ["Installed"]);
}

#[test]
fn only_show_in_and_not_show_in_follow_the_active_desktop_names() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(
        &root,
        "only-gnome.desktop",
        "[Desktop Entry]\nType=Application\nName=GNOME Only\nOnlyShowIn=GNOME;\nExec=/bin/true\n",
    );
    write_entry(
        &root,
        "not-gnome.desktop",
        "[Desktop Entry]\nType=Application\nName=Not GNOME\nNotShowIn=GNOME;\nExec=/bin/true\n",
    );
    write_entry(
        &root,
        "only-kde.desktop",
        "[Desktop Entry]\nType=Application\nName=KDE Only\nOnlyShowIn=KDE;\nExec=/bin/true\n",
    );

    let scanner = DesktopEntryScanner::with_environment(
        vec![root],
        vec!["GNOME".to_owned(), "Unity".to_owned()],
        Vec::new(),
    );
    let discovery: &dyn ApplicationDiscovery = &scanner;
    let applications = discovery.discover().expect("desktop filtering succeeds");

    assert_eq!(names(&applications), ["GNOME Only"]);
}

#[test]
fn localized_name_uses_locale_fallback_before_the_base_name() {
    let scratch = Scratch::new();
    let root = scratch.root("applications");
    write_entry(
        &root,
        "localized.desktop",
        "[Desktop Entry]\nType=Application\nName=English\nName[fr]=Français\nName[de]=Deutsch\nExec=/bin/true\n",
    );

    let scanner =
        DesktopEntryScanner::with_environment(vec![root], Vec::new(), vec!["fr_FR.UTF-8".to_owned()]);
    let discovery: &dyn ApplicationDiscovery = &scanner;
    let applications = discovery.discover().expect("localized discovery succeeds");

    assert_eq!(names(&applications), ["Français"]);
}

#[test]
fn the_linux_backend_reports_application_discovery_as_available() {
    let scratch = Scratch::new();
    let configured = LinuxBackend::with_application_roots(vec![scratch.root("applications")]);

    assert_eq!(LinuxBackend::NAME, "linux");
    assert_eq!(
        configured.capability(Capability::ApplicationDiscovery),
        CapabilityState::Available
    );
    assert_eq!(
        LinuxBackend::new().capability(Capability::ApplicationDiscovery),
        CapabilityState::Available
    );
}

/// Nothing without a Linux implementation is ever claimed, and the three
/// session-dependent capabilities answer for the session they were built for.
///
/// Built through `with_desktop_environment` rather than `new()`, because
/// capability reporting is session aware: asserting one blanket answer would
/// pass on a headless runner and fail under X11, which makes the assertion a
/// statement about the runner instead of about the backend.
#[test]
fn capabilities_without_a_linux_implementation_report_unavailable() {
    // File search is deliberately absent: it now has an implementation
    // (`file_search.rs`), and what it reports depends on the running user's
    // `$HOME` and on whether `plocate` is installed. Asserting it here would be
    // a statement about the build host; `capabilities.rs` pins it instead, with
    // the service injected. The clipboard is absent for the neighbouring reason:
    // it has an implementation (`clipboard.rs`) and what it may claim depends on
    // the session, so `capabilities.rs` pins it per session instead.
    let unimplemented = [
        Capability::UriOpen,
        Capability::Notifications,
        Capability::FileWatching,
        Capability::SecretStorage,
        Capability::ShellIntegration,
    ];
    // Global shortcuts rest on X11 `GrabKey`, or on the `GlobalShortcuts`
    // portal under Wayland (ADR-0011); window control additionally needs an
    // EWMH window manager, which the session label cannot promise, so X11
    // claims it only as `Partial` (spec 18.2, 18.6).
    let hotkeys_only = [Capability::GlobalHotkeys];
    let window_control = [Capability::WindowEnumeration, Capability::WindowActivation];
    // Icons need no display server at all, so the answer is the same in every
    // session, and it is `Partial` rather than `Available` because `.svgz`,
    // `.xpm` and scaled theme directories are not decoded.
    let icons = [Capability::Icons];

    // The portal answer is injected rather than probed: a Wayland row that
    // consulted the build host's session bus would assert something about the
    // runner, which is the same trap `with_desktop_environment` exists for.
    for (environment, portal, expected_hotkeys, expected_windows) in [
        (
            DesktopEnvironment::X11,
            false,
            CapabilityState::Available,
            CapabilityState::Partial,
        ),
        (
            DesktopEnvironment::Wayland,
            true,
            CapabilityState::Available,
            CapabilityState::UnsupportedDesktopEnvironment,
        ),
        (
            DesktopEnvironment::Wayland,
            false,
            CapabilityState::Unavailable,
            CapabilityState::UnsupportedDesktopEnvironment,
        ),
        (
            DesktopEnvironment::Headless,
            false,
            CapabilityState::Unavailable,
            CapabilityState::Unavailable,
        ),
    ] {
        let backend = LinuxBackend::with_desktop_environment_and_portal(environment, portal);
        for capability in unimplemented {
            assert_eq!(
                backend.capability(capability),
                CapabilityState::Unavailable,
                "{capability:?} has no Linux implementation yet and must not be claimed under \
                 {environment:?} (spec 18.2)"
            );
        }
        for capability in icons {
            assert_eq!(
                backend.capability(capability),
                CapabilityState::Partial,
                "{capability:?} resolves themed names in any session, including {environment:?}, \
                 but not every theme asset (spec 18.2)"
            );
        }
        for (group, expected) in [
            (&hotkeys_only[..], expected_hotkeys),
            (&window_control[..], expected_windows),
        ] {
            for &capability in group {
                assert_eq!(
                    backend.capability(capability),
                    expected,
                    "{capability:?} under {environment:?} must report what that session can actually \
                     deliver (spec 18.2, 18.6)"
                );
            }
        }
    }
}
