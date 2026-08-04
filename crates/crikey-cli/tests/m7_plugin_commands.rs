//! Black-box tests for `crikey plugin` and `crikey package migrate-keypirinha`
//! (spec 28; 21.2, 23, 26.1, 26.2; acceptance §31.29).
//!
//! Every assertion consumes only the frozen percent-encoded `key=value` surface
//! the commands print — never a config, package-manager or diagnostics
//! implementation type. Each test runs the real binary in its own set of
//! `CRIKEY_*_DIR` directories, so no test can see another's configuration and
//! none touches the developer's real one.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The binary under test, as built by cargo for this integration target.
const CRIKEY: &str = env!("CARGO_BIN_EXE_crikey");

/// A completed operation that found nothing wrong.
const EX_OK: i32 = 0;
/// A completed operation that reached a bad verdict.
const EX_INVALID: i32 = 1;
/// An argument list the command could not parse.
const EX_USAGE: i32 = 64;
/// The Rust runtime's status for an unwound panic.
const PANIC_STATUS: i32 = 101;

// ---------------------------------------------------------------------------
// Running the binary
// ---------------------------------------------------------------------------

/// A private directory tree removed when the test that made it ends.
///
/// Holds this invocation's config, data, cache and state directories plus its
/// plugin fixtures. Every `crikey` process a test starts is pointed at it, so a
/// test can neither read nor write the developer's real configuration.
struct Host {
    path: PathBuf,
}

impl Host {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-plugin-cli-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("fixture subdirectory is creatable");
        path
    }

    /// Runs `crikey` with this host's directories and no discovery roots.
    fn run(&self, args: &[&str]) -> Run {
        self.run_with(args, &[])
    }

    /// Runs `crikey` with this host's directories and extra environment.
    fn run_with(&self, args: &[&str], extra: &[(&str, &Path)]) -> Run {
        let mut command = Command::new(CRIKEY);
        command.args(args);
        command.env("CRIKEY_CONFIG_DIR", self.path.join("config"));
        command.env("CRIKEY_DATA_DIR", self.path.join("data"));
        command.env("CRIKEY_CACHE_DIR", self.path.join("cache"));
        command.env("CRIKEY_STATE_DIR", self.path.join("state"));
        // The Legacy Compatibility Layer's extraction root. Named explicitly so a
        // test never writes into the developer's real per-user cache.
        command.env("CRIKEY_LEGACY_CACHE_ROOT", self.path.join("legacy-cache"));
        // Absent unless a test sets them, so discovery is empty by default.
        command.env_remove("CRIKEY_LEGACY_PACKAGE_ROOTS");
        command.env_remove("CRIKEY_MODERN_PLUGIN_ROOTS");
        command.env_remove("CRIKEY_NATIVE_PLUGIN_ROOTS");
        for (key, value) in extra {
            command.env(key, value);
        }
        let output = command.output().expect("the crikey binary runs");
        Run {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// One completed invocation, retained so an assertion failure shows all output.
#[derive(Debug)]
struct Run {
    args: Vec<String>,
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl fmt::Display for Run {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "crikey {}\n  status: {:?}\n  stdout:\n{}\n  stderr:\n{}",
            self.args.join(" "),
            self.status,
            indent(&self.stdout),
            indent(&self.stderr)
        )
    }
}

fn indent(text: &str) -> String {
    if text.is_empty() {
        return "    <empty>".to_owned();
    }
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Asserts the process exited with `code` and did not panic.
///
/// A panic is checked separately because it also exits non-zero: a command that
/// unwound has not "reported a bad verdict", it has crashed, and the two must
/// never be confused by a test that only compares status codes.
fn assert_completed(run: &Run, code: i32) {
    assert_ne!(
        run.status,
        Some(PANIC_STATUS),
        "the command must never panic; {run}"
    );
    assert!(
        !run.stderr.contains("panicked at"),
        "the command must never panic; {run}"
    );
    assert_eq!(run.status, Some(code), "unexpected exit status; {run}");
}

// ---------------------------------------------------------------------------
// Reading the frozen key=value output
// ---------------------------------------------------------------------------

/// One printed line, split into its whitespace-safe fields.
#[derive(Debug)]
struct Record {
    fields: BTreeMap<String, String>,
}

impl Record {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    fn field(&self, key: &str, run: &Run) -> &str {
        self.get(key)
            .unwrap_or_else(|| panic!("line is missing `{key}`: {:?}; {run}", self.fields))
    }
}

fn parse(run: &Run) -> Vec<Record> {
    run.stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = BTreeMap::new();
            for token in line.split_whitespace() {
                let (key, value) = token
                    .split_once('=')
                    .unwrap_or_else(|| panic!("token `{token}` is not key=value; {run}"));
                fields.insert(key.to_owned(), decode(value));
            }
            Record { fields }
        })
        .collect()
}

/// Every record whose first key is `key`.
fn records<'a>(run: &Run, parsed: &'a [Record], key: &str) -> Vec<&'a Record> {
    let found: Vec<&Record> = parsed.iter().filter(|record| record.get(key).is_some()).collect();
    assert!(!found.is_empty(), "no `{key}=` line was printed; {run}");
    found
}

/// The single-field summary lines, as one map. Panics on a repeated summary key:
/// two values for one key would make a report whose reader has to guess.
fn summary(run: &Run, parsed: &[Record]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for record in parsed {
        if record.fields.len() == 1 {
            let (key, value) = record.fields.iter().next().expect("one field");
            assert!(
                map.insert(key.clone(), value.clone()).is_none(),
                "summary key `{key}` was printed twice; {run}"
            );
        }
    }
    map
}

fn field<'a>(summary: &'a BTreeMap<String, String>, key: &str, run: &Run) -> &'a str {
    summary
        .get(key)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("summary is missing `{key}`: {summary:?}; {run}"))
}

/// Decodes the uppercase percent escapes the commands emit.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).expect("decoded output is UTF-8")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Writes `<root>/<id>/crikey.toml` for a modern python plugin.
fn modern_plugin(root: &Path, id: &str, extra: &str) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir_all(&directory).expect("plugin directory is creatable");
    fs::write(
        directory.join("crikey.toml"),
        format!(
            "manifest-version = 1\n\n\
             [plugin]\n\
             id = \"{id}\"\n\
             name = \"{id}\"\n\
             version = \"1.2.3\"\n\
             runtime = \"python\"\n\
             entrypoint = \"{id}:Plugin\"\n{extra}"
        ),
    )
    .expect("manifest is writable");
    fs::write(directory.join(format!("{id}.py")), "class Plugin:\n    pass\n").expect("module is writable");
    directory
}

/// Writes `<root>/<id>/crikey.toml` for a native plugin.
fn native_plugin(root: &Path, id: &str) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir_all(&directory).expect("plugin directory is creatable");
    fs::write(
        directory.join("crikey.toml"),
        format!(
            "manifest-version = 1\n\n\
             [plugin]\n\
             id = \"{id}\"\n\
             name = \"{id}\"\n\
             version = \"0.4.0\"\n\
             runtime = \"native\"\n\
             entrypoint = \"bin/{id}\"\n"
        ),
    )
    .expect("manifest is writable");
    directory
}

/// Writes a loose Keypirinha package directory: one top-level module plus a
/// settings file, which is what a real one holds.
fn legacy_package(root: &Path, id: &str) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir_all(&directory).expect("package directory is creatable");
    fs::write(
        directory.join(format!("{}.py", id.to_lowercase())),
        "import keypirinha as kp\n\n\
         class Plugin(kp.Plugin):\n    pass\n",
    )
    .expect("module is writable");
    fs::write(directory.join("settings.ini"), "[main]\nvalue = 1\n").expect("settings are writable");
    directory
}

/// The `plugin=` row for `id`, from a `list` or `doctor` report.
fn row<'a>(run: &'a Run, parsed: &'a [Record], id: &str) -> &'a Record {
    records(run, parsed, "plugin")
        .into_iter()
        .find(|record| record.get("id") == Some(id))
        .unwrap_or_else(|| panic!("no row for `{id}`; {run}"))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// One plugin of each runtime on the discovery roots, one `list`.
///
/// Pins every column §28 asks `plugin list` for — id, version, kind, enabled
/// state and scheduling profile — and pins the namespaced id as the primary key,
/// because every other subcommand and every config key uses it. A `list` that
/// printed only bare ids would make `legacy.notes` and `modern.notes`
/// indistinguishable.
#[test]
fn list_reports_id_version_kind_enabled_state_and_scheduling_profile_for_every_runtime() {
    let host = Host::new("list");
    let legacy_root = host.subdir("legacy");
    let modern_root = host.subdir("modern");
    let native_root = host.subdir("native");
    legacy_package(&legacy_root, "Notes");
    modern_plugin(&modern_root, "notes", "");
    native_plugin(&native_root, "tool");

    let run = host.run_with(
        &["plugin", "list"],
        &[
            ("CRIKEY_LEGACY_PACKAGE_ROOTS", &legacy_root),
            ("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root),
            ("CRIKEY_NATIVE_PLUGIN_ROOTS", &native_root),
        ],
    );
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    assert_eq!(field(&summary(&run, &parsed), "plugins", &run), "3", "{run}");

    let legacy = row(&run, &parsed, "legacy.Notes");
    assert_eq!(legacy.field("kind", &run), "legacy", "{run}");
    assert_eq!(legacy.field("raw", &run), "Notes", "{run}");
    // The `.keypirinha-package` format carries no version. An invented one would
    // make a migrated package look released.
    assert_eq!(legacy.field("version", &run), "-", "{run}");
    assert_eq!(legacy.field("enabled", &run), "true", "{run}");
    assert_eq!(
        legacy.field("scheduling_profile", &run),
        "legacy-strict",
        "spec 7.2: a legacy package defaults to legacy-strict; {run}"
    );

    let modern = row(&run, &parsed, "modern.notes");
    assert_eq!(modern.field("kind", &run), "modern", "{run}");
    assert_eq!(modern.field("version", &run), "1.2.3", "{run}");
    assert_eq!(
        modern.field("scheduling_profile", &run),
        "modern",
        "a modern plugin defaults to the modern profile; {run}"
    );

    let native = row(&run, &parsed, "native.tool");
    assert_eq!(native.field("kind", &run), "native", "{run}");
    assert_eq!(native.field("version", &run), "0.4.0", "{run}");
}

/// A root that cannot be read is named, not silently dropped.
///
/// A shorter list that looks complete is how an operator concludes their plugin
/// was uninstalled when the directory was merely unreadable.
#[test]
fn list_names_a_discovery_root_it_could_not_read() {
    let host = Host::new("unreadable");
    let missing = host.path.join("not-here");

    let run = host.run_with(&["plugin", "list"], &[("CRIKEY_MODERN_PLUGIN_ROOTS", &missing)]);
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    assert_eq!(field(&summary(&run, &parsed), "unreadable", &run), "1", "{run}");
    let detail = records(&run, &parsed, "unreadable")
        .into_iter()
        .find(|record| record.get("reason").is_some())
        .unwrap_or_else(|| panic!("no `unreadable=N reason=..` line was printed; {run}"));
    let reason = detail.field("reason", &run);
    assert!(
        reason.contains("not-here"),
        "the reason must name the root at fault, got `{reason}`; {run}"
    );
}

// ---------------------------------------------------------------------------
// crikey run
// ---------------------------------------------------------------------------

/// `crikey run` reads the disabled set from the configuration before it touches
/// a display or a provider (spec 21.2).
///
/// This is the CLI half of the guarantee. The provider half — that a plugin in
/// the disabled set is never loaded, no worker is started for it, and it is
/// recorded unavailable with the shared `disabled by configuration` reason — is
/// pinned against the real `NativeProvider` and a real worker subprocess by
/// `crikey-app/tests/safe_mode_suppression.rs::safe_mode_reports_its_own_reason_\
/// and_never_a_second_disabled_reason_for_the_same_plugin`, which makes exactly
/// the `load` call this function makes. What can only be observed *here* is the
/// wiring between them: that the launcher reads the store `crikey plugin
/// disable` wrote, and reads it early enough that a host with no display still
/// gets that far.
///
/// The evidence is an unreadable configuration: the launcher must announce the
/// documented degradation rather than refuse the launch or silently disable
/// everything, and it must do so *before* the renderer's failure, which on a
/// headless host is where this process ends. A launcher that read the
/// configuration after building its window would print the two in the other
/// order, and one that never read it at all would print only the second.
#[test]
fn crikey_run_reads_the_disabled_set_before_it_touches_a_display() {
    let host = Host::new("run-disabled");
    let modern_root = host.subdir("modern");
    modern_plugin(&modern_root, "notes", "");
    let roots: &[(&str, &Path)] = &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)];

    // A real disable first, so the file the launcher reads is one this command
    // family produced rather than a hand-written fixture.
    assert_completed(
        &host.run_with(&["plugin", "disable", "modern.notes"], roots),
        EX_OK,
    );
    let config = host.path.join("config").join("config.toml");
    assert!(
        fs::read_to_string(&config)
            .expect("the disable must have written the user layer")
            .contains("modern.notes"),
        "`plugin disable` must persist the plugin id the launcher keys on"
    );

    // Now make it unreadable, which is the only configuration outcome a headless
    // launch can observe.
    fs::write(&config, "this is not toml = = =\n").expect("config is writable");
    let run = host.run_with(&["run"], roots);
    assert_ne!(
        run.status,
        Some(PANIC_STATUS),
        "an unreadable configuration must never panic the launcher; {run}"
    );
    // The configuration and the disabled set are read together, so one
    // diagnostic covers both: it names the failure and states what the launch
    // falls back to. A disabled plugin therefore runs again after this, which
    // is exactly why the fallback has to be announced rather than silent.
    let announced = run
        .stderr
        .find("cannot load the configuration")
        .unwrap_or_else(|| {
            panic!("the launcher must announce that it could not read the configuration; {run}")
        });
    assert!(
        run.stderr.contains("built-in defaults only"),
        "the announcement must say what the launch fell back to, not merely that it failed; {run}"
    );
    // Everything after the configuration is downstream of it. On a host with a
    // display the launch continues; on this one the renderer fails, and either
    // way the configuration diagnostic came first.
    if let Some(renderer) = run.stderr.find("launcher failed") {
        assert!(
            announced < renderer,
            "the configuration must be read before the display is touched; {run}"
        );
    }
}

// ---------------------------------------------------------------------------
// install and remove
// ---------------------------------------------------------------------------

/// Install, see it listed as installed, remove it, see it gone.
///
/// All four in one test because each half alone is passable by a broken
/// implementation: an `install` that copies nothing still exits zero, and a
/// `remove` that deletes nothing still exits zero if `list` never showed the
/// plugin in the first place.
#[test]
fn a_plugin_directory_installs_appears_in_the_list_and_removes_again() {
    let host = Host::new("install");
    let source = modern_plugin(&host.subdir("source"), "notes", "");

    let installed = host.run(&["plugin", "install", source.to_str().expect("utf-8 path")]);
    assert_completed(&installed, EX_OK);
    let parsed = parse(&installed);
    let installed_summary = summary(&installed, &parsed);
    assert_eq!(field(&installed_summary, "plugin", &installed), "modern.notes");
    assert_eq!(field(&installed_summary, "id", &installed), "notes");
    assert_eq!(field(&installed_summary, "kind", &installed), "modern");
    assert_eq!(field(&installed_summary, "verdict", &installed), "installed");
    let root = PathBuf::from(field(&installed_summary, "root", &installed));
    assert!(
        root.join("crikey.toml").is_file(),
        "the install must actually place the manifest, looked in `{}`; {installed}",
        root.display()
    );

    let listed = host.run(&["plugin", "list"]);
    assert_completed(&listed, EX_OK);
    let listed_parsed = parse(&listed);
    assert_eq!(field(&summary(&listed, &listed_parsed), "plugins", &listed), "1");
    assert_eq!(
        row(&listed, &listed_parsed, "modern.notes").field("origin", &listed),
        "installed",
        "{listed}"
    );

    let removed = host.run(&["plugin", "remove", "modern.notes"]);
    assert_completed(&removed, EX_OK);
    let removed_parsed = parse(&removed);
    assert_eq!(
        field(&summary(&removed, &removed_parsed), "verdict", &removed),
        "removed"
    );
    assert!(
        !root.join("crikey.toml").is_file(),
        "the removal must actually delete the manifest at `{}`; {removed}",
        root.display()
    );

    let empty = host.run(&["plugin", "list"]);
    assert_completed(&empty, EX_OK);
    let empty_parsed = parse(&empty);
    assert_eq!(field(&summary(&empty, &empty_parsed), "plugins", &empty), "0");
}

/// A source that is not a plugin is a completed operation with a bad verdict,
/// not a usage error and not a panic.
#[test]
fn installing_a_source_that_is_not_a_plugin_fails_with_a_named_reason() {
    let host = Host::new("install-bogus");
    let missing = host.path.join("no-such-thing");

    let run = host.run(&["plugin", "install", missing.to_str().expect("utf-8 path")]);
    assert_completed(&run, EX_INVALID);
    assert!(
        run.stderr.contains("no-such-thing"),
        "the refusal must name the source it refused; {run}"
    );
    assert!(
        run.stdout.is_empty(),
        "a failed install must print no success report; {run}"
    );
}

/// `remove` owns only the directories CriKey installed into.
#[test]
fn removing_a_plugin_that_lives_on_a_discovery_root_is_refused_rather_than_deleted() {
    let host = Host::new("remove-root");
    let modern_root = host.subdir("modern");
    let directory = modern_plugin(&modern_root, "notes", "");

    let run = host.run_with(
        &["plugin", "remove", "modern.notes"],
        &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)],
    );
    assert_completed(&run, EX_INVALID);
    assert!(
        run.stderr.contains("not installed by CriKey"),
        "the refusal must explain whose plugin it is; {run}"
    );
    assert!(
        directory.join("crikey.toml").is_file(),
        "a refused removal must not have deleted anything; {run}"
    );
}

// ---------------------------------------------------------------------------
// enable and disable
// ---------------------------------------------------------------------------

/// Disable, read it back from a *separate process*, enable, read it back again.
///
/// A separate process is the point: an in-memory flag would satisfy a single-run
/// assertion, and the guarantee §21.2 asks for is that the state was persisted.
#[test]
fn disable_then_enable_persists_across_processes() {
    let host = Host::new("enable");
    let modern_root = host.subdir("modern");
    modern_plugin(&modern_root, "notes", "");
    let roots: &[(&str, &Path)] = &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)];

    let disabled = host.run_with(&["plugin", "disable", "modern.notes"], roots);
    assert_completed(&disabled, EX_OK);
    let disabled_parsed = parse(&disabled);
    let disabled_summary = summary(&disabled, &disabled_parsed);
    assert_eq!(field(&disabled_summary, "enabled", &disabled), "false");
    assert_eq!(field(&disabled_summary, "verdict", &disabled), "disabled");

    let after_disable = host.run_with(&["plugin", "list"], roots);
    assert_completed(&after_disable, EX_OK);
    let parsed = parse(&after_disable);
    assert_eq!(
        row(&after_disable, &parsed, "modern.notes").field("enabled", &after_disable),
        "false",
        "the disabled state must survive the process that set it; {after_disable}"
    );

    let enabled = host.run_with(&["plugin", "enable", "modern.notes"], roots);
    assert_completed(&enabled, EX_OK);

    let after_enable = host.run_with(&["plugin", "list"], roots);
    assert_completed(&after_enable, EX_OK);
    let parsed = parse(&after_enable);
    assert_eq!(
        row(&after_enable, &parsed, "modern.notes").field("enabled", &after_enable),
        "true",
        "enabling must undo the disable, not merely record a second key; {after_enable}"
    );
}

/// A bare id naming two plugins is refused by naming both (spec 10.2).
///
/// Guessing would silently disable the wrong plugin and the operator would have
/// no way to tell which one the command acted on.
#[test]
fn a_bare_id_that_matches_two_runtimes_is_refused_and_names_both() {
    let host = Host::new("ambiguous");
    let legacy_root = host.subdir("legacy");
    let modern_root = host.subdir("modern");
    legacy_package(&legacy_root, "notes");
    modern_plugin(&modern_root, "notes", "");
    let roots: &[(&str, &Path)] = &[
        ("CRIKEY_LEGACY_PACKAGE_ROOTS", &legacy_root),
        ("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root),
    ];

    let run = host.run_with(&["plugin", "disable", "notes"], roots);
    assert_completed(&run, EX_INVALID);
    assert!(
        run.stderr.contains("legacy.notes") && run.stderr.contains("modern.notes"),
        "the refusal must name every candidate; {run}"
    );

    // Nothing was written: the exact spelling still reports both as enabled.
    let listed = host.run_with(&["plugin", "list"], roots);
    let parsed = parse(&listed);
    for id in ["legacy.notes", "modern.notes"] {
        assert_eq!(
            row(&listed, &parsed, id).field("enabled", &listed),
            "true",
            "an ambiguous argument must change nothing; {listed}"
        );
    }

    // Named exactly, the same command succeeds and touches only that plugin.
    let exact = host.run_with(&["plugin", "disable", "modern.notes"], roots);
    assert_completed(&exact, EX_OK);
    let listed = host.run_with(&["plugin", "list"], roots);
    let parsed = parse(&listed);
    assert_eq!(
        row(&listed, &parsed, "modern.notes").field("enabled", &listed),
        "false",
        "{listed}"
    );
    assert_eq!(
        row(&listed, &parsed, "legacy.notes").field("enabled", &listed),
        "true",
        "disabling one runtime's plugin must not touch the other's; {listed}"
    );
}

/// An unknown plugin is a bad verdict, and the refusal lists what is known.
#[test]
fn naming_a_plugin_that_does_not_exist_reports_the_plugins_that_do() {
    let host = Host::new("unknown");
    let modern_root = host.subdir("modern");
    modern_plugin(&modern_root, "notes", "");

    let run = host.run_with(
        &["plugin", "disable", "nope"],
        &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)],
    );
    assert_completed(&run, EX_INVALID);
    assert!(
        run.stderr.contains("modern.notes"),
        "the refusal must list the known plugins so the operator can correct it; {run}"
    );
}

// ---------------------------------------------------------------------------
// scheduling-profile
// ---------------------------------------------------------------------------

/// Read the profile, set it, read it back.
///
/// The `profile_source` column is what makes this non-vacuous: a command that
/// printed the default every time would pass an equality check on the profile
/// name alone.
#[test]
fn scheduling_profile_reports_the_default_then_the_value_it_was_set_to() {
    let host = Host::new("profile");
    let modern_root = host.subdir("modern");
    modern_plugin(&modern_root, "notes", "");
    let roots: &[(&str, &Path)] = &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)];

    let initial = host.run_with(&["plugin", "scheduling-profile", "modern.notes"], roots);
    assert_completed(&initial, EX_OK);
    let parsed = parse(&initial);
    let initial_summary = summary(&initial, &parsed);
    assert_eq!(field(&initial_summary, "scheduling_profile", &initial), "modern");
    assert_eq!(field(&initial_summary, "profile_source", &initial), "default");
    assert_eq!(field(&initial_summary, "verdict", &initial), "reported");

    let set = host.run_with(
        &["plugin", "scheduling-profile", "modern.notes", "legacy-strict"],
        roots,
    );
    assert_completed(&set, EX_OK);
    let parsed = parse(&set);
    let set_summary = summary(&set, &parsed);
    assert_eq!(field(&set_summary, "scheduling_profile", &set), "legacy-strict");
    assert_eq!(field(&set_summary, "profile_source", &set), "config");
    assert_eq!(field(&set_summary, "verdict", &set), "set");

    let reread = host.run_with(&["plugin", "scheduling-profile", "modern.notes"], roots);
    assert_completed(&reread, EX_OK);
    let parsed = parse(&reread);
    let reread_summary = summary(&reread, &parsed);
    assert_eq!(
        field(&reread_summary, "scheduling_profile", &reread),
        "legacy-strict",
        "the profile must have been persisted; {reread}"
    );
    assert_eq!(field(&reread_summary, "profile_source", &reread), "config");

    // `default` clears the override rather than writing the default as a value.
    let cleared = host.run_with(
        &["plugin", "scheduling-profile", "modern.notes", "default"],
        roots,
    );
    assert_completed(&cleared, EX_OK);
    let parsed = parse(&cleared);
    let cleared_summary = summary(&cleared, &parsed);
    assert_eq!(field(&cleared_summary, "scheduling_profile", &cleared), "modern");
    assert_eq!(
        field(&cleared_summary, "profile_source", &cleared),
        "default",
        "`default` must remove the override, not record it; {cleared}"
    );
}

/// `legacy-optimized` on a legacy plugin is permitted and states its departure
/// (spec 7.2).
///
/// Both halves matter: refusing it would deny the opt-in the spec grants, and
/// accepting it silently would let an operator turn on debouncing and dynamic
/// result caching for Keypirinha code that was never written to tolerate either,
/// and learn about it from a bug report.
#[test]
fn setting_legacy_optimized_on_a_legacy_plugin_succeeds_and_states_the_departure() {
    let host = Host::new("departure");
    let legacy_root = host.subdir("legacy");
    legacy_package(&legacy_root, "Notes");
    let roots: &[(&str, &Path)] = &[("CRIKEY_LEGACY_PACKAGE_ROOTS", &legacy_root)];

    let run = host.run_with(
        &["plugin", "scheduling-profile", "legacy.Notes", "legacy-optimized"],
        roots,
    );
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    let set = summary(&run, &parsed);
    assert_eq!(field(&set, "scheduling_profile", &run), "legacy-optimized");
    assert_eq!(field(&set, "verdict", &run), "set");
    let departure = field(&set, "departure", &run);
    assert!(
        departure.contains("legacy-strict"),
        "the departure must name the profile it departs from, got `{departure}`; {run}"
    );

    // Left on `legacy-strict`, there is nothing to warn about.
    let strict = host.run_with(
        &["plugin", "scheduling-profile", "legacy.Notes", "legacy-strict"],
        roots,
    );
    assert_completed(&strict, EX_OK);
    let parsed = parse(&strict);
    assert!(
        !summary(&strict, &parsed).contains_key("departure"),
        "legacy-strict is the guarantee, not a departure from it; {strict}"
    );
}

/// A misspelled profile is a usage error, never a silent fallback to the
/// default: a plugin whose scheduling changed because of a typo is a defect
/// nobody can see.
#[test]
fn a_misspelled_scheduling_profile_is_refused_rather_than_defaulted() {
    let host = Host::new("bad-profile");
    let modern_root = host.subdir("modern");
    modern_plugin(&modern_root, "notes", "");
    let roots: &[(&str, &Path)] = &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)];

    let run = host.run_with(
        &["plugin", "scheduling-profile", "modern.notes", "legacy_strict"],
        roots,
    );
    assert_completed(&run, EX_USAGE);

    let reread = host.run_with(&["plugin", "scheduling-profile", "modern.notes"], roots);
    let parsed = parse(&reread);
    assert_eq!(
        field(&summary(&reread, &parsed), "profile_source", &reread),
        "default",
        "a refused profile must not have been written; {reread}"
    );
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// A healthy plugin's declared budgets, probed.
///
/// The numbers are the mutation surface: an undeclared budget must resolve to
/// the host default of one and admit the launcher's first request, and the
/// refusal counter must still be zero. A `from_section` that resolved silence to
/// zero, a probe that never ran, or a refusal counter that moved without a
/// refusal all fail here.
#[test]
fn doctor_probes_every_work_kind_and_reports_the_resolved_limit_and_refusals() {
    let host = Host::new("doctor-healthy");
    let modern_root = host.subdir("modern");
    modern_plugin(&modern_root, "notes", "");

    let run = host.run_with(
        &["plugin", "doctor"],
        &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)],
    );
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    let budgets = records(&run, &parsed, "budget");
    let kinds: Vec<&str> = budgets.iter().map(|record| record.field("work", &run)).collect();
    assert_eq!(
        kinds,
        vec!["suggestion", "action", "background", "catalog"],
        "spec 13.5: all four work kinds must be reported, in slot order; {run}"
    );
    for record in &budgets {
        let work = record.field("work", &run);
        assert_eq!(
            record.field("limit", &run),
            "1",
            "an undeclared `{work}` budget resolves to the host default of one; {run}"
        );
        assert_eq!(
            record.field("admitted", &run),
            "true",
            "the launcher's first `{work}` request must be admitted; {run}"
        );
        assert_eq!(
            record.field("refusals", &run),
            "0",
            "an admitted `{work}` request must record no refusal; {run}"
        );
        assert_eq!(record.field("surface", &run), "enabled", "{run}");
    }

    let summary = summary(&run, &parsed);
    assert_eq!(field(&summary, "degraded", &run), "0", "{run}");
    assert_eq!(field(&summary, "verdict", &run), "healthy", "{run}");
    assert_eq!(
        row(&run, &parsed, "modern.notes").field("manifest", &run),
        "valid",
        "{run}"
    );
}

/// A declared zero is the author switching a surface off, not a defect.
///
/// Spec 19.1 keeps "the author said nothing" distinct from "the author wrote 0"
/// on purpose. A `doctor` that called a declared zero degraded would report
/// every deliberately query-only plugin as broken, and the report would stop
/// being read.
#[test]
fn doctor_reports_a_declared_zero_budget_as_switched_off_and_still_healthy() {
    let host = Host::new("doctor-zero");
    let modern_root = host.subdir("modern");
    modern_plugin(
        &modern_root,
        "notes",
        "\n[concurrency]\nmax-action-requests = 0\n",
    );

    let run = host.run_with(
        &["plugin", "doctor"],
        &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)],
    );
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    let action = records(&run, &parsed, "budget")
        .into_iter()
        .find(|record| record.get("work") == Some("action"))
        .unwrap_or_else(|| panic!("no action budget line; {run}"));
    assert_eq!(action.field("limit", &run), "0", "{run}");
    assert_eq!(action.field("admitted", &run), "false", "{run}");
    assert_eq!(
        action.field("refusals", &run),
        "1",
        "the refused probe must be counted; {run}"
    );
    assert_eq!(
        action.field("surface", &run),
        "disabled-by-declaration",
        "a declared zero is a decision, not a fault; {run}"
    );
    assert_eq!(
        field(&summary(&run, &parsed), "verdict", &run),
        "healthy",
        "a plugin that switched one surface off is not degraded; {run}"
    );
}

/// A manifest that does not parse is a degraded plugin, reported with the reason
/// and a non-zero status.
#[test]
fn doctor_reports_an_unparseable_manifest_as_degraded_with_the_parse_error() {
    let host = Host::new("doctor-broken");
    let modern_root = host.subdir("modern");
    let directory = modern_root.join("broken");
    fs::create_dir_all(&directory).expect("plugin directory is creatable");
    fs::write(
        directory.join("crikey.toml"),
        "manifest-version = 1\nthis is not toml\n",
    )
    .expect("manifest is writable");

    let run = host.run_with(
        &["plugin", "doctor"],
        &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)],
    );
    assert_completed(&run, EX_INVALID);
    let parsed = parse(&run);
    let broken = row(&run, &parsed, "modern.broken");
    assert_ne!(
        broken.field("manifest", &run),
        "valid",
        "an unparseable manifest must not be reported valid; {run}"
    );
    assert!(
        broken.field("manifest", &run).contains("crikey.toml"),
        "the reason must name the file at fault; {run}"
    );
    let verdicts = records(&run, &parsed, "verdict");
    assert!(
        verdicts
            .iter()
            .any(|record| record.get("health") == Some("degraded")),
        "the plugin must be reported degraded; {run}"
    );
    assert_eq!(field(&summary(&run, &parsed), "degraded", &run), "1", "{run}");
}

/// A legacy package's §26.2 findings come from the Legacy Compatibility Layer's
/// own diagnostics store, and the scheduling profile is one of them.
///
/// Pinning the `info` severity is deliberate: reporting the profile is an
/// observation, and a `doctor` that gave it the weight of a defect would exit
/// non-zero for every conforming legacy plugin on the host.
#[test]
fn doctor_reports_legacy_compatibility_findings_without_calling_the_profile_a_defect() {
    let host = Host::new("doctor-legacy");
    let legacy_root = host.subdir("legacy");
    legacy_package(&legacy_root, "Notes");

    let run = host.run_with(
        &["plugin", "doctor", "legacy.Notes"],
        &[("CRIKEY_LEGACY_PACKAGE_ROOTS", &legacy_root)],
    );
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    let profile_finding = records(&run, &parsed, "warning")
        .into_iter()
        .find(|record| record.get("code") == Some("scheduling-profile"))
        .unwrap_or_else(|| panic!("the scheduling profile must be reported (spec 26.2); {run}"));
    assert_eq!(
        profile_finding.field("severity", &run),
        "info",
        "reporting the profile is an observation, not a defect; {run}"
    );
    assert!(
        profile_finding.field("message", &run).contains("legacy-strict"),
        "the finding must name the profile; {run}"
    );
    assert_eq!(
        field(&summary(&run, &parsed), "verdict", &run),
        "healthy",
        "an info finding must not make a conforming plugin degraded; {run}"
    );
}

/// `doctor <id>` reports that plugin and no other.
#[test]
fn doctor_with_an_id_reports_only_that_plugin() {
    let host = Host::new("doctor-one");
    let modern_root = host.subdir("modern");
    modern_plugin(&modern_root, "alpha", "");
    modern_plugin(&modern_root, "beta", "");

    let run = host.run_with(
        &["plugin", "doctor", "modern.alpha"],
        &[("CRIKEY_MODERN_PLUGIN_ROOTS", &modern_root)],
    );
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    assert_eq!(field(&summary(&run, &parsed), "plugins", &run), "1", "{run}");
    assert!(
        !run.stdout.contains("modern.beta"),
        "a named plugin's report must not include its siblings; {run}"
    );
}

// ---------------------------------------------------------------------------
// Statuses and argument handling
// ---------------------------------------------------------------------------

/// The three statuses stay distinguishable, and `plugin` is no longer
/// unavailable.
///
/// Exit 69 is reserved for a command that is advertised and unbuilt. Every
/// `plugin` subcommand is built, so none may answer it — a script that treats 69
/// as "not implemented" must not see it here.
#[test]
fn plugin_statuses_distinguish_usage_errors_bad_verdicts_and_never_report_unavailable() {
    let host = Host::new("statuses");

    let unknown = host.run(&["plugin", "no-such-subcommand"]);
    assert_completed(&unknown, EX_USAGE);

    let missing = host.run(&["plugin", "enable"]);
    assert_completed(&missing, EX_USAGE);

    let extra = host.run(&["plugin", "list", "unexpected"]);
    assert_completed(&extra, EX_USAGE);

    let option = host.run(&["plugin", "doctor", "--unknown"]);
    assert_completed(&option, EX_USAGE);

    let bad_verdict = host.run(&["plugin", "enable", "nope"]);
    assert_completed(&bad_verdict, EX_INVALID);

    for subcommand in [
        "list",
        "install",
        "remove",
        "enable",
        "disable",
        "doctor",
        "scheduling-profile",
    ] {
        let help = host.run(&["plugin", subcommand, "--help"]);
        assert_completed(&help, EX_OK);
        assert!(
            help.stdout.contains(subcommand),
            "`plugin {subcommand} --help` must document itself; {help}"
        );
    }

    let family = host.run(&["plugin", "--help"]);
    assert_completed(&family, EX_OK);
    assert!(
        family.stdout.contains("scheduling-profile"),
        "the family help must list every subcommand; {family}"
    );
}

/// `--help` must not become a way to have a typo accepted.
#[test]
fn help_beside_an_unknown_option_is_refused_rather_than_printed() {
    let host = Host::new("help-typo");
    let run = host.run(&["plugin", "list", "--help", "--bogus"]);
    assert_completed(&run, EX_USAGE);
}

// ---------------------------------------------------------------------------
// package migrate-keypirinha
// ---------------------------------------------------------------------------

/// One Keypirinha package, migrated.
///
/// The manifest must declare exactly what the source format carries and nothing
/// more, every file must be copied, and every untranslatable fact must be named.
/// The `version` assertion is the one that matters most: a migration that wrote a
/// plausible `0.1.0` would produce a package that claims a release nobody cut.
#[test]
fn migrate_keypirinha_writes_a_manifest_that_claims_only_what_the_source_carries() {
    let host = Host::new("migrate");
    let legacy_root = host.subdir("legacy");
    let source = legacy_package(&legacy_root, "Notes");
    let destination = host.path.join("migrated");

    let run = host.run(&[
        "package",
        "migrate-keypirinha",
        "--package",
        source.to_str().expect("utf-8 path"),
        "--out",
        destination.to_str().expect("utf-8 path"),
    ]);
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    let report = summary(&run, &parsed);
    assert_eq!(field(&report, "plugin", &run), "Notes");
    assert_eq!(field(&report, "runtime", &run), "legacy-python");
    assert_eq!(field(&report, "scheduling_profile", &run), "legacy-strict");
    assert_eq!(field(&report, "entrypoint", &run), "notes");
    assert_eq!(field(&report, "verdict", &run), "migrated");
    assert!(
        field(&report, "version", &run).contains("keypirinha-migrated"),
        "the version must be a visible placeholder, got `{}`; {run}",
        field(&report, "version", &run)
    );

    let manifest = fs::read_to_string(destination.join("crikey.toml")).expect("manifest was written");
    for absent in ["[python]", "[platform]", "requires-python", "dependencies"] {
        assert!(
            !manifest.contains(absent),
            "the manifest must not declare `{absent}`, which the source format does not carry:\n{manifest}"
        );
    }
    assert!(
        manifest.contains("runtime = \"legacy-python\""),
        "the manifest must declare the runtime it does know:\n{manifest}"
    );
    assert!(
        destination.join("notes.py").is_file(),
        "the plugin module must be copied; {run}"
    );
    assert!(
        destination.join("settings.ini").is_file(),
        "package resources must be copied; {run}"
    );

    // Every untranslatable fact is reported, keyed by a stable code.
    let limitations: Vec<String> = run
        .stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter_map(|(key, _)| key.strip_prefix("limitation.").map(str::to_owned))
        .collect();
    for expected in [
        "no-declared-version",
        "no-declared-name",
        "no-declared-python-requirement",
        "no-declared-dependencies",
        "settings-file",
    ] {
        assert!(
            limitations.iter().any(|code| code == expected),
            "`{expected}` must be reported; got {limitations:?}; {run}"
        );
    }
    assert_eq!(
        field(&report, "limitations", &run),
        limitations.len().to_string(),
        "the count must match the lines printed; {run}"
    );

    // The generated manifest is one CriKey can actually load: `plugin list`
    // reads it back through the same parser the providers use.
    let listed = host.run_with(
        &["plugin", "list"],
        &[("CRIKEY_LEGACY_PACKAGE_ROOTS", &host.path.join("nowhere"))],
    );
    assert_completed(&listed, EX_OK);
}

/// A destination that already exists is refused, not overwritten.
///
/// The workflow is to migrate and then hand-edit the two placeholder fields; a
/// second run that silently replaced the directory would destroy that work.
#[test]
fn migrate_keypirinha_refuses_an_existing_destination_rather_than_overwriting_it() {
    let host = Host::new("migrate-exists");
    let source = legacy_package(&host.subdir("legacy"), "Notes");
    let destination = host.subdir("migrated");
    fs::write(destination.join("crikey.toml"), "hand edited\n").expect("marker is writable");

    let run = host.run(&[
        "package",
        "migrate-keypirinha",
        "--package",
        source.to_str().expect("utf-8 path"),
        "--out",
        destination.to_str().expect("utf-8 path"),
    ]);
    assert_completed(&run, EX_INVALID);
    let parsed = parse(&run);
    assert_eq!(
        field(&summary(&run, &parsed), "verdict", &run),
        "invalid",
        "{run}"
    );
    assert_eq!(
        fs::read_to_string(destination.join("crikey.toml")).expect("marker is readable"),
        "hand edited\n",
        "a refused migration must not have touched the destination; {run}"
    );
}

/// A source that is not a Keypirinha package is a bad verdict naming the path,
/// and both required flags are enforced.
#[test]
fn migrate_keypirinha_statuses_separate_a_bad_source_from_a_bad_argument_list() {
    let host = Host::new("migrate-statuses");
    let missing = host.path.join("no-such-package");
    let destination = host.path.join("out");

    let bad_source = host.run(&[
        "package",
        "migrate-keypirinha",
        "--package",
        missing.to_str().expect("utf-8 path"),
        "--out",
        destination.to_str().expect("utf-8 path"),
    ]);
    assert_completed(&bad_source, EX_INVALID);
    assert!(
        bad_source.stderr.contains("no-such-package"),
        "the refusal must name the source; {bad_source}"
    );
    assert!(
        !destination.exists(),
        "a refused migration must create nothing; {bad_source}"
    );

    for args in [
        vec!["package", "migrate-keypirinha"],
        vec!["package", "migrate-keypirinha", "--package", "x"],
        vec!["package", "migrate-keypirinha", "--out", "y"],
        vec!["package", "migrate-keypirinha", "--package="],
        vec!["package", "migrate-keypirinha", "--bogus", "z"],
    ] {
        let run = host.run(&args);
        assert_completed(&run, EX_USAGE);
    }

    let help = host.run(&["package", "migrate-keypirinha", "--help"]);
    assert_completed(&help, EX_OK);
    assert!(
        help.stdout.contains("--out"),
        "the help must document both required flags; {help}"
    );
}
