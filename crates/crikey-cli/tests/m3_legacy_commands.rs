//! Black-box tests for `crikey dev test-legacy-compat`, `crikey dev
//! inspect-catalog` and `crikey dev compatibility-report` (spec 26.3, 28, 14.5,
//! 14.8, 14.9, 14.10, 14.12, 27.4; roadmap M3; acceptance 31.11-31.18, 31.29,
//! 31.31).
//!
//! These drive the built binary the way a developer does — arguments in, exit
//! status and stdout out — and deliberately reach into no workspace library
//! type. `crikey dev test-legacy-compat` is the command a maintainer runs before
//! writing a corpus classification into
//! `compatibility/real-plugin-corpus/corpus.toml`; if its contract is a Rust
//! enum rather than an exit status and a column of text, it cannot be run from
//! the shell that produced the classification.
//!
//! # The output contract
//!
//! Every line is whitespace-separated `key=value` tokens, the shape `crikey dev
//! benchmark`, `trace-query` and `simulate-typing` already use, so `cut`, `grep`
//! and `sort` are a complete reader.
//!
//! * A line with a `check=<name>` token is one *conformance check*.
//! * A line with an `item=<index>` token is one *catalog item*.
//! * Every other non-blank line contributes to the run *summary*.
//!
//! Legacy packages contain human text — item labels are written by plugin
//! authors, not by us — so a value may need to hold a space, an `=` or a `%`.
//! Values are therefore percent-encoded with uppercase hex, which keeps
//! `split_whitespace` then `split_once('=')` a total reader while staying
//! lossless. Replacing spaces with underscores would also parse, and would
//! quietly corrupt the one thing catalog inspection exists to show you.
//!
//! # Why the exit status has three meanings and not two
//!
//! A conformance *failure* is a result, not a refusal: the command ran, learned
//! something, and must print all of it. So a failing verdict exits 1 with the
//! full report on stdout, while a bad argument list exits `EX_USAGE` with an
//! empty stdout, and `EX_UNAVAILABLE` is reserved for a subcommand that is
//! advertised but unbuilt. These three must never be confused, because a CI job
//! that cannot distinguish "this plugin is incompatible" from "you typed the
//! flag wrong" from "we never wrote this command" reports all three as red.
//!
//! A third check result, `unavailable`, exists for the same reason. This host is
//! Linux; a check backed by Win32 cannot be run here and must say so rather than
//! passing vacuously (roadmap principle 7). A run with no failures but some
//! unavailable checks is `incomplete`, not `pass`, and still exits non-zero: a
//! green tick that means "we did not look" is the plausible lie the roadmap
//! forbids.
//!
//! # Determinism
//!
//! Nothing here sleeps or samples a clock, and the fixtures are the synthetic
//! packages committed under `compatibility/test-plugins/`. Two runs of one
//! invocation must be byte-identical, including the failing ones — a conformance
//! report that varies between runs cannot be diffed against the last release,
//! which is the only use a compatibility corpus has.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The binary under test, as built by cargo for this integration target.
const CRIKEY: &str = env!("CARGO_BIN_EXE_crikey");

/// A clean run: the command did what it was asked and found nothing wrong.
const EX_OK: i32 = 0;
/// A conformance run that completed and reached a non-passing verdict. The
/// report is still on stdout; only the status differs.
const EX_NOT_CONFORMANT: i32 = 1;
/// `EX_USAGE`: the caller's fault, and the only failure a bad argument list or
/// an unloadable package may produce.
const EX_USAGE: i32 = 64;
/// `EX_UNAVAILABLE`: what `crikey dev` answers for a subcommand it advertises
/// but has not built. An implemented command must never answer with it.
const EX_UNAVAILABLE: i32 = 69;
/// The exit status the Rust runtime uses for an unwound panic.
const PANIC_STATUS: i32 = 101;

/// The three M3 developer commands of spec 26.3 and 28.
const M3_COMMANDS: [&str; 3] = ["test-legacy-compat", "inspect-catalog", "compatibility-report"];

/// The two that take a package to work on.
const PACKAGE_COMMANDS: [&str; 2] = ["test-legacy-compat", "inspect-catalog"];

// ---------------------------------------------------------------------------
// Fixture packages (compatibility/test-plugins)
// ---------------------------------------------------------------------------

/// Conforms to every legacy scheduling rule of spec 14.5 and 14.8: it polls
/// `should_terminate()`, caches nothing dynamic, imports only portable
/// compatibility modules, and publishes a small catalog.
const CONFORMING: &str = "well-behaved";
/// Identical to `well-behaved` except that its `on_suggest` runs long and never
/// reads `should_terminate()` (spec 9.2, 27.3, acceptance 31.17). One rule
/// broken, so exactly one check may fail.
const IGNORES_TERMINATION: &str = "ignores-should-terminate";
/// Identical to `well-behaved` except that it serves dynamic suggestions from a
/// module-level cache across requests (spec 14.9, acceptance 31.18).
const CACHES_SUGGESTIONS: &str = "caches-dynamic-suggestions";
/// Conforms to every scheduling rule, but imports `keypirinha_wintypes` at
/// module scope and reaches a Win32 entry point only behind
/// `if keypirinha_wintypes.is_available():` (spec 14.2, 14.12, acceptance
/// 31.31).
///
/// Loading and scheduling therefore succeed on every host — importing the shim
/// is documented to work everywhere — while the Win32 behaviour itself cannot be
/// exercised here. That split is the whole point: the package is correct and
/// simply not portable, and the report has to be able to say both.
///
/// The guard spelling is load-bearing. `hasattr(kpwt, "kernel32")` and
/// `getattr(kpwt, "kernel32", None)` do *not* work: the shim raises
/// `WindowsOnlyError` on attribute access and it is deliberately a
/// `RuntimeError` rather than an `AttributeError`, precisely so those two probes
/// cannot launder a Win32 access into a silent `False` or `None`. A fixture
/// written that way would fail to load here instead of loading and honestly
/// reporting the check as unavailable, which is the behaviour under test.
const WINDOWS_ONLY: &str = "windows-only";
/// Publishes a fixed three-item catalog and no dynamic suggestions. One label
/// contains a space, a `%` and an `=`, so the output encoding is exercised by
/// the fixture rather than only by assertion.
const CATALOG_ONLY: &str = "catalog-only";

// ---------------------------------------------------------------------------
// The conformance suite
// ---------------------------------------------------------------------------

/// Checks every legacy package is put through, whatever it contains.
///
/// One named check per rule, because "this package failed legacy conformance" is
/// not a bug report: the maintainer's next question is always which rule, and a
/// blanket verdict sends them back to reading the spec to guess.
const CORE_CHECKS: [&str; 13] = [
    // Spec 5.1, 16.1: the interpreter is a child process, never the UI thread.
    "worker_runs_out_of_process",
    // Spec 14.5, acceptance 31.15.
    "initial_query_broadcast",
    "selected_item_routed_to_owner",
    // Spec 14.5, acceptance 31.14.
    "host_time_debounce_disabled",
    "host_gating_disabled",
    // Spec 14.5, 14.8, acceptance 31.16.
    "callbacks_serialized_per_instance",
    // Spec 8.4, 14.5.
    "obsolete_work_replaced",
    // Spec 9.2, acceptance 31.17.
    "should_terminate_observed",
    // Spec 8.5, acceptance 31.7.
    "stale_results_rejected",
    // Spec 14.9, acceptance 31.18.
    "dynamic_suggestions_not_cached",
    // Spec 14.8.
    "repeated_on_catalog_permitted",
    "obsolete_catalog_updates_rejected",
    // Spec 14.12, acceptance 31.31.
    "windows_only_dependencies_declared",
];

/// Emitted only for a package that declares a Windows-only dependency: there is
/// nothing to say about Win32 entry points for a package that never names one.
const WIN32_CHECK: &str = "win32_entry_points_operational";

const CHECK_RESULTS: [&str; 3] = ["pass", "fail", "unavailable"];
const VERDICTS: [&str; 3] = ["pass", "fail", "incomplete"];

/// Summary fields every `test-legacy-compat` run reports.
const CONFORMANCE_SUMMARY: [&str; 13] = [
    "command",
    "package",
    "package_id",
    "platform",
    "interpreter",
    "python_version",
    "scheduling_profile",
    "portable",
    "checks_total",
    "checks_passed",
    "checks_failed",
    "checks_unavailable",
    "verdict",
];

/// Summary fields every `inspect-catalog` run reports.
const CATALOG_SUMMARY: [&str; 7] = [
    "command",
    "package",
    "package_id",
    "interpreter",
    "python_version",
    "scheduling_profile",
    "items",
];

/// Fields every catalog item line carries (spec 10.1).
const ITEM_FIELDS: [&str; 11] = [
    "item",
    "id",
    "category",
    "label",
    "description",
    "target",
    "search_terms",
    "argument_policy",
    "hit_policy",
    "score_hint",
    "actions",
];

/// Built-in item categories of spec 10.3. The fixture uses only these, so a
/// value outside the set means the category slug was invented somewhere.
const CATEGORIES: [&str; 9] = [
    "application",
    "file",
    "directory",
    "url",
    "command",
    "expression",
    "keyword",
    "contact",
    "clipboard-item",
];

const ARGUMENT_POLICIES: [&str; 3] = ["forbidden", "optional", "required"];
const HIT_POLICIES: [&str; 2] = ["recorded", "ignored"];

// ---------------------------------------------------------------------------
// The compatibility report (spec 14.10, 27.4)
// ---------------------------------------------------------------------------

const MATRIX_PATH: &str = "compatibility/api-matrix/matrix.toml";
const CORPUS_PATH: &str = "compatibility/real-plugin-corpus/corpus.toml";

/// The six API classifications of spec 14.10, as report keys. Their counts must
/// sum to `matrix_apis`.
const MATRIX_CLASSES: [&str; 6] = [
    "matrix_full",
    "matrix_behavioural_difference",
    "matrix_windows_only",
    "matrix_partial",
    "matrix_unsupported",
    "matrix_planned",
];

/// The nine plugin classifications of spec 27.4 plus `untested`, as report keys.
///
/// `untested` is counted rather than omitted: a corpus that silently drops the
/// packages nobody has run yet reports better coverage than it has.
const CORPUS_CLASSES: [&str; 10] = [
    "corpus_works_unchanged",
    "corpus_works_with_configuration_changes",
    "corpus_works_with_minimal_source_changes",
    "corpus_windows_only_but_compatible",
    "corpus_blocked_missing_apis",
    "corpus_blocked_python_version",
    "corpus_blocked_undocumented_behaviour",
    "corpus_works_only_under_legacy_optimized",
    "corpus_requires_legacy_strict",
    "corpus_untested",
];

// ---------------------------------------------------------------------------
// Running the binary
// ---------------------------------------------------------------------------

/// One completed invocation, kept whole so a failing assertion can print the
/// command that produced it alongside everything it said.
#[derive(Debug)]
struct Run {
    args: Vec<String>,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl fmt::Display for Run {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "\n$ crikey {args}\nexit: {code}\n--- stdout ---\n{stdout}--- stderr ---\n{stderr}---",
            args = self.args.join(" "),
            code = match self.code {
                Some(code) => code.to_string(),
                None => "killed by signal".to_owned(),
            },
            stdout = self.stdout,
            stderr = self.stderr,
        )
    }
}

fn run_without(args: &[&str], removed: &[&str]) -> Run {
    let mut command = Command::new(CRIKEY);
    command.args(args);
    for name in removed {
        command.env_remove(name);
    }

    let output = command
        .output()
        .unwrap_or_else(|error| panic!("could not execute `{CRIKEY}` with {args:?}: {error}"));

    Run {
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)
            .unwrap_or_else(|_| panic!("`crikey {args:?}` wrote non-UTF-8 to stdout")),
        stderr: String::from_utf8(output.stderr)
            .unwrap_or_else(|_| panic!("`crikey {args:?}` wrote non-UTF-8 to stderr")),
    }
}

fn run(args: &[&str]) -> Run {
    run_without(args, &[])
}

/// Runs the binary and requires it to have succeeded with something to say.
fn succeed(args: &[&str]) -> Run {
    let run = run(args);
    assert!(
        !run.stderr.contains("panicked at"),
        "a developer command must not panic{run}"
    );
    assert_eq!(run.code, Some(EX_OK), "expected success{run}");
    assert!(
        !run.stdout.trim().is_empty(),
        "a successful developer command must report what it did{run}"
    );
    run
}

// ---------------------------------------------------------------------------
// Reading the output
// ---------------------------------------------------------------------------

/// One printed line, split into its `key=value` tokens.
#[derive(Debug)]
struct Record {
    line: usize,
    fields: Vec<(String, String)>,
}

impl Record {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn need(&self, key: &str, run: &Run) -> &str {
        self.get(key).unwrap_or_else(|| {
            panic!(
                "line {line}: `{label}` has no `{key}` field{run}",
                line = self.line,
                label = self
                    .get("check")
                    .or_else(|| self.get("item"))
                    .unwrap_or("<summary>"),
            )
        })
    }

    fn number(&self, key: &str, run: &Run) -> u64 {
        let raw = self.need(key, run);
        raw.parse().unwrap_or_else(|_| {
            panic!(
                "line {line}: `{key}={raw}` is not a whole number{run}",
                line = self.line,
            )
        })
    }

    /// A line describing one repeated thing rather than the run as a whole.
    fn is_detail(&self) -> bool {
        self.get("check").is_some() || self.get("item").is_some()
    }
}

/// Splits stdout into records, requiring every token to be `key=value` with a
/// value that needs no quoting.
///
/// Strict on purpose: one unsplittable token is enough to make the whole stream
/// unreadable by the shell pipeline the format exists for, and a developer
/// discovers that at the moment they need the report.
fn parse(run: &Run) -> Vec<Record> {
    run.stdout
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let number = index + 1;
            let mut seen = BTreeSet::new();
            let fields = line
                .split_whitespace()
                .map(|token| {
                    let (key, value) = token
                        .split_once('=')
                        .unwrap_or_else(|| panic!("line {number}: token `{token}` is not `key=value`{run}"));
                    assert!(
                        !key.is_empty(),
                        "line {number}: token `{token}` has an empty key{run}"
                    );
                    assert!(
                        !value.contains('='),
                        "line {number}: `{token}` puts a bare `=` in its value, so splitting on \
                         the first `=` and splitting on the last disagree; encode it as \
                         `%3D`{run}"
                    );
                    assert!(
                        seen.insert(key.to_owned()),
                        "line {number}: repeats the key `{key}`, so a reader cannot tell which \
                         value is meant{run}"
                    );
                    (key.to_owned(), value.to_owned())
                })
                .collect::<Vec<_>>();
            assert!(!fields.is_empty(), "line {number}: no fields{run}");
            Record { line: number, fields }
        })
        .collect()
}

/// Every summary field of the run, refusing a field reported twice.
fn summary(records: &[Record], run: &Run) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for record in records.iter().filter(|record| !record.is_detail()) {
        for (key, value) in &record.fields {
            let previous = fields.insert(key.clone(), value.clone());
            assert!(
                previous.is_none(),
                "line {line}: the summary reports `{key}` twice; a reader cannot tell which run \
                 it describes{run}",
                line = record.line,
            );
        }
    }
    fields
}

fn field<'a>(summary: &'a BTreeMap<String, String>, key: &str, run: &Run) -> &'a str {
    summary
        .get(key)
        .unwrap_or_else(|| panic!("the run summary has no `{key}` field{run}"))
        .as_str()
}

fn number(summary: &BTreeMap<String, String>, key: &str, run: &Run) -> u64 {
    let raw = field(summary, key, run);
    raw.parse()
        .unwrap_or_else(|_| panic!("summary `{key}={raw}` is not a whole number{run}"))
}

fn checks(records: &[Record]) -> Vec<&Record> {
    records
        .iter()
        .filter(|record| record.get("check").is_some())
        .collect()
}

fn items(records: &[Record]) -> Vec<&Record> {
    records
        .iter()
        .filter(|record| record.get("item").is_some())
        .collect()
}

fn check_named<'a>(records: &'a [Record], name: &str, run: &Run) -> &'a Record {
    let present: Vec<&str> = checks(records)
        .into_iter()
        .filter_map(|record| record.get("check"))
        .collect();
    let mut matching = checks(records)
        .into_iter()
        .filter(|record| record.get("check") == Some(name));
    let found = matching
        .next()
        .unwrap_or_else(|| panic!("no `{name}` check was run; the suite ran {present:?}{run}"));
    assert!(
        matching.next().is_none(),
        "`{name}` was reported more than once, so its result is ambiguous{run}"
    );
    found
}

/// Decodes one printed value back to the text it stands for.
///
/// The encoding is percent-escaping with uppercase hex, applied to anything that
/// would otherwise break the format. Decoding here rather than eyeballing the
/// escapes is what makes the round trip — and therefore the losslessness —
/// actually asserted.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value
                .get(index + 1..index + 3)
                .unwrap_or_else(|| panic!("`{value}` ends inside a percent escape"));
            assert!(
                hex.chars()
                    .all(|digit| digit.is_ascii_digit() || digit.is_ascii_uppercase()),
                "`{value}` uses lowercase hex in `%{hex}`; one spelling per escape or two \
                 encoders will disagree"
            );
            let byte = u8::from_str_radix(hex, 16)
                .unwrap_or_else(|_| panic!("`{value}` contains the invalid escape `%{hex}`"));
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| panic!("`{value}` decodes to invalid UTF-8"))
}

// ---------------------------------------------------------------------------
// Fixtures on disk
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/crikey-cli sits two levels below the workspace root")
        .to_path_buf()
}

/// The absolute path of a committed synthetic package.
///
/// A missing fixture fails here rather than skipping: a conformance suite that
/// quietly tests nothing when its packages are absent is worse than no suite,
/// because it is green.
fn fixture(name: &str) -> String {
    let path = workspace_root().join("compatibility/test-plugins").join(name);
    assert!(
        path.is_dir(),
        "the synthetic legacy package `{name}` is missing from {}; these tests do not skip",
        path.display(),
    );
    path.into_os_string()
        .into_string()
        .expect("the workspace path is not valid UTF-8")
}

/// A directory this test owns and removes, holding the paths that must *not*
/// load as legacy packages.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-m3-cli-{pid}-{ordinal}-{label}",
            pid = std::process::id(),
            ordinal = NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", path.display()));
        Self { path }
    }

    fn text(&self, name: &str, contents: &str) -> String {
        let path = self.path.join(name);
        fs::write(&path, contents)
            .unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));
        display(&path)
    }

    fn directory(&self, name: &str) -> String {
        let path = self.path.join(name);
        fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", path.display()));
        display(&path)
    }

    fn absent(&self, name: &str) -> String {
        display(&self.path.join(name))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn display(path: &Path) -> String {
    path.to_str()
        .unwrap_or_else(|| panic!("{} is not valid UTF-8", path.display()))
        .to_owned()
}

/// Runs the conformance suite over a fixture and returns the parsed report.
///
/// Accepts both verdict statuses: a package that fails a rule is exactly what
/// this command exists to find, and its report is as much a result as a clean
/// one.
fn conformance(name: &str) -> (Run, Vec<Record>) {
    let package = fixture(name);
    let run = run(&["dev", "test-legacy-compat", "--package", &package]);

    assert!(
        !run.stderr.contains("panicked at"),
        "the conformance suite panicked on `{name}`{run}"
    );
    assert_ne!(
        run.code,
        Some(EX_UNAVAILABLE),
        "`dev test-legacy-compat` is implemented, so it must never answer {EX_UNAVAILABLE}{run}"
    );
    assert!(
        matches!(run.code, Some(EX_OK | EX_NOT_CONFORMANT)),
        "a completed conformance run exits {EX_OK} or {EX_NOT_CONFORMANT}{run}"
    );
    assert!(
        !run.stdout.trim().is_empty(),
        "a conformance run must print what it found, whichever verdict it reached{run}"
    );

    let records = parse(&run);
    (run, records)
}

fn catalog(name: &str) -> (Run, Vec<Record>) {
    let package = fixture(name);
    // No display server, so a command that tried to open the launcher window
    // would fail here rather than quietly succeeding on a developer's desktop.
    let run = run_without(
        &["dev", "inspect-catalog", "--package", &package],
        &["DISPLAY", "WAYLAND_DISPLAY"],
    );
    assert!(
        !run.stderr.contains("panicked at"),
        "catalog inspection panicked on `{name}`{run}"
    );
    assert_eq!(
        run.code,
        Some(EX_OK),
        "`{name}` is a loadable package, so inspecting its catalog must succeed{run}"
    );
    let records = parse(&run);
    (run, records)
}

/// The platform slug this build reports for itself.
fn host_platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

fn parse_version(raw: &str, run: &Run) -> (u64, u64, u64) {
    let parts: Vec<&str> = raw.split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "`python_version={raw}` is not `major.minor.patch`{run}"
    );
    let mut numbers = parts.iter().map(|part| {
        part.parse::<u64>()
            .unwrap_or_else(|_| panic!("`python_version={raw}` has a non-numeric component{run}"))
    });
    (
        numbers.next().expect("three components"),
        numbers.next().expect("three components"),
        numbers.next().expect("three components"),
    )
}

/// Assertions every package-scoped run owes, whichever command produced it.
fn assert_package_identity(summary: &BTreeMap<String, String>, name: &str, command: &str, run: &Run) {
    assert_eq!(field(summary, "command", run), command);
    assert_eq!(
        decode(field(summary, "package", run)),
        fixture(name),
        "the report must echo the package it was pointed at, or a saved report cannot be traced \
         back to its input{run}"
    );
    assert_eq!(
        field(summary, "package_id", run),
        name,
        "a legacy package is identified by its directory name (spec 14.3){run}"
    );
    assert_eq!(
        field(summary, "scheduling_profile", run),
        "legacy-strict",
        "unchanged legacy packages run under `legacy-strict` (spec 7.1, acceptance 31.14){run}"
    );

    // Spec 5.1 and 16.1: the interpreter is a real child process. Naming it is
    // what lets a developer check which one was used when a package behaves
    // differently on two machines.
    let interpreter = decode(field(summary, "interpreter", run));
    assert!(
        Path::new(&interpreter).is_file(),
        "`interpreter={interpreter}` does not name an interpreter on this host{run}"
    );
    let version = parse_version(field(summary, "python_version", run), run);
    assert!(
        version >= (3, 8, 0),
        "the layer's floor is CPython 3.8; the run reports {version:?}{run}"
    );
}

// ---------------------------------------------------------------------------
// The commands exist and describe themselves
// ---------------------------------------------------------------------------

#[test]
fn every_m3_developer_command_is_implemented_rather_than_advertised() {
    for command in M3_COMMANDS {
        let run = run(&["dev", command, "--help"]);
        assert_ne!(
            run.code,
            Some(EX_UNAVAILABLE),
            "`dev {command}` is listed in the usage text, so it must do something{run}"
        );
        assert_ne!(
            run.code,
            Some(PANIC_STATUS),
            "`dev {command}` must not answer with a panic{run}"
        );
        assert!(
            !run.stderr.contains("panicked at"),
            "`dev {command}` panicked{run}"
        );
    }
}

#[test]
fn help_explains_each_command_without_inspecting_a_package() {
    for command in M3_COMMANDS {
        for flag in ["-h", "--help"] {
            let run = succeed(&["dev", command, flag]);

            assert!(
                run.stdout.contains(command),
                "help for `{command}` does not name the command{run}"
            );
            assert!(
                run.stdout.contains("USAGE"),
                "help for `{command}` does not show how to invoke it{run}"
            );
            for line in run.stdout.lines() {
                let first = line.split_whitespace().next().unwrap_or_default();
                for marker in ["check=", "item=", "verdict=", "matrix_apis="] {
                    assert!(
                        !first.starts_with(marker),
                        "`dev {command} {flag}` emitted a `{marker}` report line, so it did the \
                         work instead of explaining it{run}"
                    );
                }
            }
        }
    }

    // The strongest available proof that help runs nothing: point the command at
    // a package that would be refused, and ask for help anyway. A command that
    // validated its arguments before honouring `--help` would exit 64 here.
    let scratch = Scratch::new("help-runs-nothing");
    let absent = scratch.absent("no-such-package");
    for command in PACKAGE_COMMANDS {
        let run = succeed(&["dev", command, "--package", &absent, "--help"]);
        assert!(
            run.stdout.contains("--package"),
            "help for `{command}` does not document the flag it cannot run without{run}"
        );
        assert!(
            !run.stdout.contains(absent.as_str()),
            "`dev {command} --help` looked at the package instead of explaining itself{run}"
        );
    }
}

// ---------------------------------------------------------------------------
// Bad input is refused, never crashed on
// ---------------------------------------------------------------------------

#[test]
fn an_unusable_argument_list_is_refused_with_usage_status() {
    let conforming = fixture(CONFORMING);
    let rejected: Vec<Vec<&str>> = vec![
        vec!["dev", "test-legacy-compat"],
        vec!["dev", "test-legacy-compat", "--package"],
        vec!["dev", "test-legacy-compat", "--package", ""],
        vec!["dev", "test-legacy-compat", "--package="],
        vec!["dev", "test-legacy-compat", &conforming],
        vec![
            "dev",
            "test-legacy-compat",
            "--package",
            &conforming,
            "--sideways",
        ],
        vec!["dev", "inspect-catalog"],
        vec!["dev", "inspect-catalog", "--package"],
        vec!["dev", "inspect-catalog", "--package", ""],
        vec!["dev", "inspect-catalog", "--package", &conforming, "--sideways"],
        vec!["dev", "compatibility-report", "--package", &conforming],
        vec!["dev", "compatibility-report", "everything"],
    ];

    for args in rejected {
        let run = run(&args);

        assert!(
            run.code.is_some(),
            "a bad argument list killed the process with a signal{run}"
        );
        assert!(
            !run.stderr.contains("panicked at"),
            "a bad argument list panicked instead of being refused{run}"
        );
        assert_eq!(
            run.code,
            Some(EX_USAGE),
            "a bad argument list must exit {EX_USAGE}{run}"
        );
        assert!(
            !run.stderr.trim().is_empty(),
            "a refusal must say what was wrong{run}"
        );
        assert!(
            run.stdout.trim().is_empty(),
            "a refused run inspected nothing, so it must report nothing on stdout{run}"
        );
    }
}

#[test]
fn a_package_path_that_cannot_be_loaded_is_refused_by_name_rather_than_by_panic() {
    let scratch = Scratch::new("unloadable");
    let unloadable = [
        // Nothing there at all.
        scratch.absent("no-such-package"),
        // A file where a package directory was expected.
        scratch.text("not-a-directory", "this is not a Keypirinha package\n"),
        // A directory with no plugin module in it.
        scratch.directory("empty-directory"),
    ];

    for command in PACKAGE_COMMANDS {
        for package in &unloadable {
            let run = run(&["dev", command, "--package", package]);

            assert!(
                !run.stderr.contains("panicked at"),
                "an unloadable package panicked `{command}` instead of being refused{run}"
            );
            assert_ne!(run.code, Some(PANIC_STATUS), "{run}");
            assert_ne!(
                run.code,
                Some(EX_UNAVAILABLE),
                "an unloadable package is the caller's problem, not an unbuilt command{run}"
            );
            assert_eq!(
                run.code,
                Some(EX_USAGE),
                "an unloadable package must be refused with {EX_USAGE}{run}"
            );
            assert!(
                run.stderr.contains(package.as_str()),
                "the refusal does not name the path it could not load{run}"
            );
            assert!(
                run.stdout.trim().is_empty(),
                "nothing was inspected, so nothing may be reported{run}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn repeating_an_invocation_reproduces_it_byte_for_byte() {
    let conforming = fixture(CONFORMING);
    let failing = fixture(IGNORES_TERMINATION);
    let catalog_package = fixture(CATALOG_ONLY);

    let invocations: Vec<Vec<&str>> = vec![
        vec!["dev", "test-legacy-compat", "--package", &conforming],
        // A failing verdict has to be as reproducible as a passing one, or the
        // corpus classification it justifies cannot be reviewed.
        vec!["dev", "test-legacy-compat", "--package", &failing],
        vec!["dev", "inspect-catalog", "--package", &catalog_package],
        vec!["dev", "compatibility-report"],
    ];

    for args in invocations {
        let first = run(&args);
        let second = run(&args);

        assert_eq!(
            first.stdout, second.stdout,
            "two runs of one invocation disagree, so something is being sampled rather than \
             computed{first}{second}"
        );
        assert_eq!(first.stderr, second.stderr, "{first}{second}");
        assert_eq!(first.code, second.code, "{first}{second}");
    }
}

#[test]
fn every_printed_line_is_unique_unquoted_key_value_pairs() {
    let conforming = fixture(CONFORMING);
    let catalog_package = fixture(CATALOG_ONLY);

    let invocations: Vec<Vec<&str>> = vec![
        vec!["dev", "test-legacy-compat", "--package", &conforming],
        vec!["dev", "inspect-catalog", "--package", &catalog_package],
        vec!["dev", "compatibility-report"],
    ];

    for args in invocations {
        let run = succeed(&args);
        // `parse` enforces the hard parts: every token splits at its first `=`,
        // no key is empty, no key repeats on a line, and no value smuggles a
        // second `=` past a naive reader.
        let records = parse(&run);
        assert!(!records.is_empty(), "a successful run printed no records{run}");

        // And no summary key may be reported twice across the whole stream.
        let summary = summary(&records, &run);
        assert!(
            !summary.is_empty(),
            "the run printed detail lines but never said what run they came from{run}"
        );

        // A shell reader splits on a single space, not on a whitespace run, and
        // does not trim. If the two disagree the format only looks readable from
        // Rust, which is the one language that will never read it in anger.
        for (index, line) in run
            .stdout
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
        {
            assert_eq!(
                line,
                line.trim(),
                "line {number}: padded with whitespace, so `cut -d' '` sees empty fields{run}",
                number = index + 1,
            );
            assert_eq!(
                line.split(' ').count(),
                line.split_whitespace().count(),
                "line {number}: fields are not separated by exactly one space{run}",
                number = index + 1,
            );
        }

        for record in &records {
            for (key, value) in &record.fields {
                // Decoding is itself the assertion: a stray `%`, a lowercase
                // escape or a truncated one all fail here, and each would make
                // two readers disagree about what the value says.
                let decoded = decode(value);
                assert_eq!(
                    decoded.is_empty(),
                    value.is_empty(),
                    "line {line}: `{key}={value}` decodes to nothing{run}",
                    line = record.line,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The conformance suite (spec 26.3)
// ---------------------------------------------------------------------------

#[test]
fn the_conformance_suite_runs_every_legacy_scheduling_check() {
    let (run, records) = conformance(CONFORMING);
    let summary = summary(&records, &run);

    for key in CONFORMANCE_SUMMARY {
        field(&summary, key, &run);
    }
    assert_package_identity(&summary, CONFORMING, "test-legacy-compat", &run);
    assert_eq!(field(&summary, "platform", &run), host_platform(), "{run}");

    let ran: BTreeSet<&str> = checks(&records)
        .into_iter()
        .filter_map(|record| record.get("check"))
        .collect();
    for required in CORE_CHECKS {
        assert!(
            ran.contains(required),
            "the suite never ran `{required}`, so the rule it defends is untested{run}"
        );
    }

    for record in checks(&records) {
        let name = record.need("check", &run);
        let result = record.need("result", &run);
        assert!(
            CHECK_RESULTS.contains(&result),
            "`{name}` answered `{result}`, which is not one of {CHECK_RESULTS:?}{run}"
        );
        if result != "pass" {
            assert!(
                !record.need("detail", &run).is_empty(),
                "`{name}` did not pass and said nothing about why{run}"
            );
        }
    }

    assert_eq!(
        checks(&records).len(),
        ran.len(),
        "a check reported twice makes its result ambiguous{run}"
    );
}

#[test]
fn the_conformance_verdict_and_its_checks_describe_the_same_run() {
    for fixture_name in [CONFORMING, IGNORES_TERMINATION, WINDOWS_ONLY] {
        let (run, records) = conformance(fixture_name);
        let summary = summary(&records, &run);
        let reported = checks(&records);

        let tally = |wanted: &str| {
            reported
                .iter()
                .filter(|record| record.get("result") == Some(wanted))
                .count() as u64
        };
        let passed = tally("pass");
        let failed = tally("fail");
        let unavailable = tally("unavailable");

        assert_eq!(
            number(&summary, "checks_total", &run),
            reported.len() as u64,
            "the summary counts checks the report does not contain{run}"
        );
        assert_eq!(number(&summary, "checks_passed", &run), passed, "{run}");
        assert_eq!(number(&summary, "checks_failed", &run), failed, "{run}");
        assert_eq!(number(&summary, "checks_unavailable", &run), unavailable, "{run}");
        assert_eq!(
            passed + failed + unavailable,
            reported.len() as u64,
            "some check was neither passed, failed nor unavailable{run}"
        );

        let verdict = field(&summary, "verdict", &run);
        assert!(
            VERDICTS.contains(&verdict),
            "`verdict={verdict}` is not one of {VERDICTS:?}{run}"
        );
        let expected = if failed > 0 {
            "fail"
        } else if unavailable > 0 {
            // Nothing broke, but we did not look everywhere. Calling that a pass
            // would report Win32 coverage this host never had.
            "incomplete"
        } else {
            "pass"
        };
        assert_eq!(
            verdict, expected,
            "the verdict and the checks disagree: {passed} passed, {failed} failed, \
             {unavailable} unavailable{run}"
        );
        assert_eq!(
            run.code,
            Some(if verdict == "pass" {
                EX_OK
            } else {
                EX_NOT_CONFORMANT
            }),
            "the exit status must agree with the verdict, or CI gates on the wrong thing{run}"
        );
    }
}

#[test]
fn a_conforming_package_passes_every_check_and_exits_successfully() {
    let (run, records) = conformance(CONFORMING);
    let summary = summary(&records, &run);

    for name in CORE_CHECKS {
        assert_eq!(
            check_named(&records, name, &run).need("result", &run),
            "pass",
            "`{CONFORMING}` is the fixture that conforms, so `{name}` must pass{run}"
        );
    }

    assert_eq!(number(&summary, "checks_failed", &run), 0, "{run}");
    assert_eq!(
        number(&summary, "checks_unavailable", &run),
        0,
        "`{CONFORMING}` declares no Windows-only dependency, so nothing about it is unrunnable \
         here{run}"
    );
    assert!(
        checks(&records)
            .iter()
            .all(|record| record.get("check") != Some(WIN32_CHECK)),
        "`{WIN32_CHECK}` is only meaningful for a package that names a Win32 entry point{run}"
    );
    assert_eq!(field(&summary, "verdict", &run), "pass", "{run}");
    assert_eq!(field(&summary, "portable", &run), "true", "{run}");
    assert_eq!(run.code, Some(EX_OK), "{run}");
}

#[test]
fn a_package_that_ignores_should_terminate_fails_that_named_check_alone() {
    let (run, records) = conformance(IGNORES_TERMINATION);
    let summary = summary(&records, &run);

    let offending = check_named(&records, "should_terminate_observed", &run);
    assert_eq!(
        offending.need("result", &run),
        "fail",
        "the fixture that never reads the flag must fail the check that watches for it{run}"
    );
    assert!(
        !offending.need("detail", &run).is_empty(),
        "a failing check has to say what it saw{run}"
    );

    for name in CORE_CHECKS {
        if name == "should_terminate_observed" {
            continue;
        }
        assert_eq!(
            check_named(&records, name, &run).need("result", &run),
            "pass",
            "`{name}` failed too, so the suite is reporting a blanket failure rather than the one \
             rule this fixture breaks{run}"
        );
    }

    assert_eq!(number(&summary, "checks_failed", &run), 1, "{run}");
    assert_eq!(field(&summary, "verdict", &run), "fail", "{run}");
    assert_eq!(
        run.code,
        Some(EX_NOT_CONFORMANT),
        "a conformance failure is a result with a status of its own, not {EX_USAGE} and not \
         {EX_UNAVAILABLE}{run}"
    );
    assert!(
        !run.stdout.trim().is_empty(),
        "the failing run must still print the whole report; the failure is the reason to read \
         it{run}"
    );
}

#[test]
fn a_package_that_caches_dynamic_suggestions_fails_only_the_caching_check() {
    let (run, records) = conformance(CACHES_SUGGESTIONS);
    let summary = summary(&records, &run);

    assert_eq!(
        check_named(&records, "dynamic_suggestions_not_cached", &run).need("result", &run),
        "fail",
        "spec 14.9 and acceptance 31.18 forbid caching dynamic legacy suggestions by default{run}"
    );
    for name in CORE_CHECKS {
        if name == "dynamic_suggestions_not_cached" {
            continue;
        }
        assert_eq!(
            check_named(&records, name, &run).need("result", &run),
            "pass",
            "`{name}` failed too, so the caching violation was reported as a blanket failure{run}"
        );
    }

    assert_eq!(number(&summary, "checks_failed", &run), 1, "{run}");
    assert_eq!(field(&summary, "verdict", &run), "fail", "{run}");
    assert_eq!(run.code, Some(EX_NOT_CONFORMANT), "{run}");
}

#[test]
fn a_windows_only_package_is_reported_honestly_and_never_as_portable() {
    let (run, records) = conformance(WINDOWS_ONLY);
    let summary = summary(&records, &run);

    // Detecting the dependency is a static fact and works on every host, so this
    // check passes here: the package does declare what it needs.
    let declared = check_named(&records, "windows_only_dependencies_declared", &run);
    assert_eq!(declared.need("result", &run), "pass", "{run}");
    assert!(
        decode(declared.need("detail", &run)).contains("keypirinha_wintypes"),
        "the check must name the dependency it found{run}"
    );

    // Exercising Win32 is not a static fact. On this host it cannot be done, and
    // saying so is the only honest answer available (roadmap principle 7).
    let win32 = check_named(&records, WIN32_CHECK, &run);
    let expected_result = if cfg!(windows) { "pass" } else { "unavailable" };
    assert_eq!(
        win32.need("result", &run),
        expected_result,
        "on {platform} the Win32 entry points must be reported as `{expected_result}`{run}",
        platform = host_platform(),
    );
    if expected_result == "unavailable" {
        assert!(
            decode(win32.need("detail", &run)).contains(host_platform()),
            "an unavailable check has to name the host that could not run it{run}"
        );
        assert_eq!(number(&summary, "checks_unavailable", &run), 1, "{run}");
        assert_eq!(
            field(&summary, "verdict", &run),
            "incomplete",
            "a run that could not perform every check has not passed{run}"
        );
        assert_eq!(run.code, Some(EX_NOT_CONFORMANT), "{run}");
    } else {
        assert_eq!(number(&summary, "checks_unavailable", &run), 0, "{run}");
        assert_eq!(field(&summary, "verdict", &run), "pass", "{run}");
        assert_eq!(run.code, Some(EX_OK), "{run}");
    }

    assert_eq!(
        number(&summary, "checks_failed", &run),
        0,
        "needing Windows is not a conformance failure; it is a portability fact{run}"
    );
    assert_eq!(
        field(&summary, "portable", &run),
        "false",
        "acceptance 31.31: a package that needs Win32 must never be presented as \
         cross-platform, on any host{run}"
    );
}

// ---------------------------------------------------------------------------
// Catalog inspection (spec 26.3, 14.8, 10.1)
// ---------------------------------------------------------------------------

#[test]
fn inspect_catalog_reports_every_item_field_without_a_display() {
    let (run, records) = catalog(CATALOG_ONLY);
    let summary = summary(&records, &run);

    for key in CATALOG_SUMMARY {
        field(&summary, key, &run);
    }
    assert_package_identity(&summary, CATALOG_ONLY, "inspect-catalog", &run);

    let listed = items(&records);
    assert_eq!(
        number(&summary, "items", &run),
        listed.len() as u64,
        "the summary counts items the report does not list{run}"
    );
    assert_eq!(
        listed.len(),
        3,
        "`{CATALOG_ONLY}` publishes a fixed three-item catalog{run}"
    );

    let mut identities = BTreeSet::new();
    for (index, record) in listed.iter().enumerate() {
        for key in ITEM_FIELDS {
            record.need(key, &run);
        }
        assert_eq!(
            record.number("item", &run),
            index as u64,
            "items are numbered from zero in publication order{run}"
        );

        let category = record.need("category", &run);
        assert!(
            CATEGORIES.contains(&category),
            "`category={category}` is not one of the built-in categories {CATEGORIES:?}{run}"
        );
        let argument_policy = record.need("argument_policy", &run);
        assert!(
            ARGUMENT_POLICIES.contains(&argument_policy),
            "`argument_policy={argument_policy}` is not one of {ARGUMENT_POLICIES:?}{run}"
        );
        let hit_policy = record.need("hit_policy", &run);
        assert!(
            HIT_POLICIES.contains(&hit_policy),
            "`hit_policy={hit_policy}` is not one of {HIT_POLICIES:?}{run}"
        );

        record.number("search_terms", &run);
        record.number("actions", &run);
        let score_hint = record.need("score_hint", &run);
        score_hint
            .parse::<i64>()
            .unwrap_or_else(|_| panic!("`score_hint={score_hint}` is not a whole number{run}"));

        let id = record.need("id", &run);
        assert!(
            identities.insert(id.to_owned()),
            "two items share the identity `{id}`, so neither can be selected reliably (spec \
             10.2){run}"
        );
        assert!(
            !decode(record.need("label", &run)).is_empty(),
            "an item with no label cannot be shown{run}"
        );
    }
}

#[test]
fn catalog_text_survives_the_key_value_encoding_intact() {
    // The fixture owns one item whose label needs every escape the format has:
    // a space that would split the token, an `=` that would split the pair, and
    // a `%` that would make decoding ambiguous if it were not itself escaped.
    const AWKWARD: &str = "Deterministic Fixture Item #2 (50% = half)";

    let (run, records) = catalog(CATALOG_ONLY);
    let listed = items(&records);

    let awkward = listed
        .iter()
        .find(|record| decode(record.need("label", &run)) == AWKWARD)
        .unwrap_or_else(|| {
            let labels: Vec<String> = listed
                .iter()
                .map(|record| decode(record.need("label", &run)))
                .collect();
            panic!("no item decoded to {AWKWARD:?}; the catalog reported {labels:?}{run}")
        });

    let raw = awkward.need("label", &run);
    for (escape, character) in [("%20", ' '), ("%3D", '='), ("%25", '%')] {
        assert!(
            raw.contains(escape),
            "`{character}` reached the output without being encoded as `{escape}`, so the label \
             was either mangled or the line cannot be split{run}"
        );
    }
    assert!(
        !raw.contains(' ') && !raw.contains('='),
        "the encoded label still breaks the format: {raw}{run}"
    );
    assert_eq!(
        decode(raw),
        AWKWARD,
        "the encoding lost or changed the label rather than escaping it{run}"
    );
}

// ---------------------------------------------------------------------------
// The compatibility report (spec 14.10, 27.4, acceptance 31.12)
// ---------------------------------------------------------------------------

#[test]
fn the_compatibility_report_counts_every_matrix_and_corpus_classification() {
    let run = succeed(&["dev", "compatibility-report"]);
    let records = parse(&run);
    let summary = summary(&records, &run);

    assert!(
        items(&records).is_empty() && checks(&records).is_empty(),
        "the report is a summary; per-API and per-package detail belong to the data files{run}"
    );

    let apis = number(&summary, "matrix_apis", &run);
    let classified: u64 = MATRIX_CLASSES.iter().map(|key| number(&summary, key, &run)).sum();
    assert_eq!(
        classified, apis,
        "the six classifications of spec 14.10 must account for every API in the matrix, or the \
         matrix has entries nobody classified{run}"
    );
    assert!(apis > 0, "an empty matrix is not a documented matrix{run}");

    let packages = number(&summary, "corpus_plugins", &run);
    let counted: u64 = CORPUS_CLASSES.iter().map(|key| number(&summary, key, &run)).sum();
    assert_eq!(
        counted, packages,
        "every corpus package must land in exactly one classification of spec 27.4, including \
         `untested`{run}"
    );
}

#[test]
fn the_compatibility_report_names_the_version_controlled_files_it_read() {
    let run = succeed(&["dev", "compatibility-report"]);
    let records = parse(&run);
    let summary = summary(&records, &run);

    for (key, expected) in [("matrix_path", MATRIX_PATH), ("corpus_path", CORPUS_PATH)] {
        let reported = decode(field(&summary, key, &run));
        assert_eq!(
            reported, expected,
            "the report must name its source as a workspace-relative path, so two people can \
             check they read the same file{run}"
        );
        let absolute = workspace_root().join(expected);
        assert!(
            absolute.is_file(),
            "`{key}` names {}, which is not in the repository; spec 14.10 requires the matrix to \
             be version-controlled{run}",
            absolute.display(),
        );
    }
}
