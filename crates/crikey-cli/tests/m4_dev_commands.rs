//! Black-box tests for `crikey dev test` and `crikey dev run`, the two developer
//! commands that exercise a *modern* Python plugin end to end (spec 26.3, 28;
//! roadmap M4; contract §7; acceptance §31.19).
//!
//! These drive the built binary the way a plugin author does — arguments in,
//! exit status and stdout out — and deliberately reach into no workspace library
//! type. The plugin, its `crikey.toml` manifest and (where a dependency is
//! involved) an offline package index are all synthesised in temp directories at
//! test time; nothing is committed. A CPython interpreter is selected by the
//! documented discovery order (`CRIKEY_PYTHON`, configured profile, then
//! `python3` on the path), and the binary discovers the SDK from the repo
//! `sdk/python` dir.
//!
//! # The output contract (identical to `dev test-legacy-compat`)
//!
//! Every line is whitespace-separated `key=value` tokens. A line carrying an
//! `item=<index>` token is one emitted result; every other non-blank line
//! contributes to the run *summary*. Plugin authors write item labels, so a
//! value may hold a space, an `=` or a `%`; values are therefore percent-encoded
//! with uppercase hex, which keeps `split_whitespace` then `split_once('=')` a
//! total reader while staying lossless. Underscoring the spaces would also parse
//! and would quietly corrupt the one thing the command exists to show.
//!
//! # Why the exit status has three meanings and not two
//!
//! A plugin that *raises* while serving a query is a completed run that found a
//! fault: it ran, learned something, and prints all of it, so it exits `1` with
//! the report still on stdout. A `--plugin` that is not a loadable modern plugin
//! is the caller's fault and exits `EX_USAGE` (64) with an empty stdout.
//! `EX_UNAVAILABLE` (69) is reserved for a subcommand that is advertised but
//! unbuilt, and an implemented command must never answer with it. A CI job that
//! cannot tell "this plugin is broken" from "you typed the flag wrong" from "we
//! never wrote this" reports all three as red.
//!
//! # Determinism
//!
//! Nothing here sleeps or samples a clock as a synchronisation primitive, and
//! every fixture is regenerated deterministically. Two runs of one invocation
//! must be byte-identical: a developer smoke that varies between runs cannot be
//! diffed and cannot be trusted.

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
/// A run that completed and found a fault (a plugin raised while serving). The
/// report is still on stdout; only the status differs.
const EX_FOUND_FAULT: i32 = 1;
/// `EX_USAGE`: the caller's fault, and the only failure a bad argument list or a
/// `--plugin` that will not load may produce.
const EX_USAGE: i32 = 64;
/// `EX_UNAVAILABLE`: what `crikey dev` answers for a subcommand it advertises
/// but has not built. An implemented command must never answer with it.
const EX_UNAVAILABLE: i32 = 69;
/// The exit status the Rust runtime uses for an unwound panic.
const PANIC_STATUS: i32 = 101;

/// The terminal run states a modern reply may report (contract §2 `BatchState`).
const RUN_STATES: [&str; 3] = ["final", "cancelled", "failed"];

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
            "\n  command: crikey {args}\n  exit:    {code:?}\n  stdout:\n{stdout}\n  stderr:\n{stderr}",
            args = self.args.join(" "),
            code = self.code,
            stdout = indent(&self.stdout),
            stderr = indent(&self.stderr),
        )
    }
}

fn indent(text: &str) -> String {
    if text.trim().is_empty() {
        return "    <empty>".to_owned();
    }
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run(args: &[&str]) -> Run {
    let output = Command::new(CRIKEY)
        .args(args)
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

/// Asserts a run was refused for a bad/unloadable argument: 64, nothing on
/// stdout, a reason on stderr, no panic.
fn assert_refused(run: &Run) {
    assert!(
        run.code.is_some(),
        "a refusal killed the process with a signal instead of exiting{run}"
    );
    assert!(
        !run.stderr.contains("panicked at"),
        "a refusal panicked instead of being reported{run}"
    );
    assert_eq!(
        run.code,
        Some(EX_USAGE),
        "a refused invocation must exit {EX_USAGE}{run}"
    );
    assert!(
        run.stdout.trim().is_empty(),
        "a refused run inspected nothing, so it must report nothing on stdout{run}"
    );
    assert!(
        !run.stderr.trim().is_empty(),
        "a refusal must say what was wrong{run}"
    );
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
        self.get(key)
            .unwrap_or_else(|| panic!("line {}: no `{key}` field{run}", self.line))
    }

    fn is_item(&self) -> bool {
        self.get("item").is_some()
    }
}

/// Splits stdout into records, requiring every token to be `key=value` with a
/// value that needs no quoting. Strict on purpose: one unsplittable token makes
/// the whole stream unreadable by the shell pipeline the format exists for.
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
                        "line {number}: `{token}` puts a bare `=` in its value; encode it as \
                         `%3D`{run}"
                    );
                    assert!(
                        seen.insert(key.to_owned()),
                        "line {number}: repeats the key `{key}`{run}"
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
    for record in records.iter().filter(|record| !record.is_item()) {
        for (key, value) in &record.fields {
            let previous = fields.insert(key.clone(), value.clone());
            assert!(
                previous.is_none(),
                "line {}: the summary reports `{key}` twice{run}",
                record.line,
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

fn items(records: &[Record]) -> Vec<&Record> {
    records.iter().filter(|record| record.is_item()).collect()
}

/// True if any printed value, once decoded, contains `needle`. Used to prove a
/// plugin-raised message actually reaches the report rather than being swallowed.
fn mentions(records: &[Record], needle: &str) -> bool {
    records.iter().any(|record| {
        record
            .fields
            .iter()
            .any(|(_, value)| decode(value).contains(needle))
    })
}

/// Decodes one printed value back to the text it stands for: percent-escaping
/// with uppercase hex. Decoding here rather than eyeballing the escapes is what
/// makes the round trip — and therefore the losslessness — actually asserted.
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
                "`{value}` uses lowercase hex in `%{hex}`; one spelling per escape"
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

fn display(path: &Path) -> String {
    path.to_str()
        .unwrap_or_else(|| panic!("{} is not valid UTF-8", path.display()))
        .to_owned()
}

/// A directory this test owns and removes, holding the plugin trees and package
/// indexes synthesised for one test.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        for _ in 0..256 {
            let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "crikey-m4-cli-{pid}-{ordinal}-{label}",
                pid = std::process::id(),
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create {}: {error}", path.display()),
            }
        }
        panic!("could not allocate a unique scratch directory for {label}");
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", path.display()));
        path
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

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", parent.display()));
    }
    fs::write(path, contents).unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));
}

/// A `[plugin]`-only manifest for a modern python plugin. `entrypoint` is the
/// single-string `module:Class` form (stored under the runtime-neutral key).
fn manifest(id: &str, entrypoint: &str) -> String {
    format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"M4 Test Plugin\"\n\
         version = \"1.0.0\"\n\
         runtime = \"python\"\n\
         entrypoint = \"{entrypoint}\"\n"
    )
}

/// Writes a plugin directory (`crikey.toml` + `plugin.py`) under `scratch` and
/// returns its absolute path for `--plugin`.
fn plugin_dir(scratch: &Scratch, name: &str, id: &str, entrypoint: &str, source: &str) -> String {
    let dir = scratch.subdir(name);
    write(&dir.join("crikey.toml"), &manifest(id, entrypoint));
    write(&dir.join("plugin.py"), source);
    display(&dir)
}

// A plugin that emits one deterministic result whose label carries a space, a
// `%` and an `=`, so the output encoding is exercised by the fixture itself.
const ECHO_SOURCE: &str = r#"from crikey_sdk import Plugin, Item


class EchoPlugin(Plugin):
    def suggest(self, query, context):
        context.emit(
            Item(
                stable_id="echo:" + query.normalized,
                label="Echo of " + query.text + " 100% = done",
                target="echo://" + query.normalized,
                description="echoed by the fixture",
            )
        )
"#;

// A plugin that raises while serving a query: transport is fine, the plugin is
// at fault, so the run completes and reports the fault.
const BOOM_SOURCE: &str = r#"from crikey_sdk import Plugin


class BoomPlugin(Plugin):
    def suggest(self, query, context):
        raise RuntimeError("boom in suggest for " + query.text)
"#;

// A cooperative plugin: it polls `context.cancelled` and returns as soon as the
// host requests cancellation. The loop's exit is driven by the cancellation
// event, not by elapsed time; the sleep merely yields the interpreter.
const COOPERATIVE_CANCEL_SOURCE: &str = r#"import time

from crikey_sdk import Plugin


class CancelPlugin(Plugin):
    def suggest(self, query, context):
        while not context.cancelled:
            time.sleep(0.001)
        context.log("cooperated with cancellation")
"#;

// A plugin that cannot be imported at all: loading it is a caller error (64),
// not a completed run that found a fault (1).
const UNIMPORTABLE_SOURCE: &str = r#"raise ImportError("this plugin refuses to import")
"#;

/// A modern plugin declaring one managed dependency it actually imports and uses
/// (acceptance §31.19). The manifest carries a `[python]` section; the worker's
/// import path must include the materialised env for the `import acme` to work.
fn dep_plugin_manifest(id: &str, entrypoint: &str, dep: &str) -> String {
    format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"M4 Dep Plugin\"\n\
         version = \"1.0.0\"\n\
         runtime = \"python\"\n\
         entrypoint = \"{entrypoint}\"\n\n\
         [python]\n\
         requires-python = \">=3.12\"\n\
         dependencies = [\"{dep}\"]\n"
    )
}

const ACME_PLUGIN_SOURCE: &str = r#"import acme

from crikey_sdk import Plugin, Item


class AcmePlugin(Plugin):
    def suggest(self, query, context):
        context.emit(
            Item(
                stable_id="acme:" + query.normalized,
                label=acme.greet(query.text),
                target="acme://" + acme.__version__,
            )
        )
"#;

/// Builds an offline package index (contract §3 layout: `<root>/<name>-<version>/`
/// each a tree of importable modules) carrying one `acme` version and returns
/// the index root path for `--index`.
fn acme_index(scratch: &Scratch, name: &str, version: &str) -> String {
    let root = scratch.subdir(name);
    let acme = root.join(format!("acme-{version}")).join("acme");
    write(
        &acme.join("__init__.py"),
        &format!(
            "__version__ = \"{version}\"\n\n\
             def greet(name):\n    return \"acme-{version} greets \" + name\n"
        ),
    );
    display(&root)
}

// ---------------------------------------------------------------------------
// The commands exist and describe themselves
// ---------------------------------------------------------------------------

#[test]
fn dev_run_and_dev_test_are_implemented_rather_than_advertised() {
    for command in ["run", "test"] {
        let help = run(&["dev", command, "--help"]);
        assert_ne!(
            help.code,
            Some(EX_UNAVAILABLE),
            "`dev {command}` is a built M4 command and must not answer EX_UNAVAILABLE{help}"
        );
        assert_ne!(
            help.code,
            Some(PANIC_STATUS),
            "`dev {command} --help` must not panic{help}"
        );
        assert!(
            !help.stderr.contains("panicked at"),
            "`dev {command} --help` panicked{help}"
        );
        assert!(
            help.stdout.contains("--plugin"),
            "help for `dev {command}` must document the flag it cannot run without{help}"
        );
    }
}

// ---------------------------------------------------------------------------
// `dev test` — loading, querying, summarising
// ---------------------------------------------------------------------------

#[test]
fn dev_test_loads_a_plugin_runs_a_query_and_summarises_the_results() {
    let scratch = Scratch::new("test-one-query");
    let plugin = plugin_dir(
        &scratch,
        "echo",
        "dev.crikey.m4.echo",
        "plugin:EchoPlugin",
        ECHO_SOURCE,
    );

    let run = succeed(&["dev", "test", "--plugin", &plugin, "--query", "foo"]);
    let records = parse(&run);
    let summary = summary(&records, &run);

    assert_eq!(
        decode(field(&summary, "plugin", &run)),
        "dev.crikey.m4.echo",
        "the summary must name the plugin it ran{run}"
    );
    assert_eq!(
        number(&summary, "queries", &run),
        1,
        "one `--query` was submitted{run}"
    );
    assert!(
        RUN_STATES.contains(&field(&summary, "state", &run)),
        "the run `state` must be one of {RUN_STATES:?}{run}"
    );
    assert_eq!(
        field(&summary, "state", &run),
        "final",
        "a plugin that emits and returns normally reaches `final`{run}"
    );

    let items = items(&records);
    assert_eq!(items.len(), 1, "the echo plugin emits exactly one result{run}");
    assert_eq!(
        number(&summary, "results", &run),
        items.len() as u64,
        "the `results` summary must count the `item=` lines it printed{run}"
    );

    let only = items[0];
    assert_eq!(
        decode(only.need("stable_id", &run)),
        "echo:foo",
        "the plugin's own stable_id must survive to the report{run}"
    );
    assert_eq!(
        decode(only.need("label", &run)),
        "Echo of foo 100% = done",
        "a label with a space, `%` and `=` must round-trip through the encoding intact{run}"
    );
}

#[test]
fn dev_test_runs_every_query_and_attributes_each_result() {
    let scratch = Scratch::new("test-two-queries");
    let plugin = plugin_dir(
        &scratch,
        "echo",
        "dev.crikey.m4.echo",
        "plugin:EchoPlugin",
        ECHO_SOURCE,
    );

    let run = succeed(&[
        "dev", "test", "--plugin", &plugin, "--query", "foo", "--query", "bar",
    ]);
    let records = parse(&run);
    let summary = summary(&records, &run);

    assert_eq!(
        number(&summary, "queries", &run),
        2,
        "both `--query` values must be run{run}"
    );

    let items = items(&records);
    assert_eq!(items.len(), 2, "each query emits one result{run}");
    assert_eq!(
        number(&summary, "results", &run),
        2,
        "the summary must count both results{run}"
    );

    // Each result is attributed to the query that produced it: the `query=`
    // token names the query, and the plugin's stable_id is derived from it.
    for query in ["foo", "bar"] {
        let attributed = items
            .iter()
            .find(|item| item.get("query").map(decode).as_deref() == Some(query))
            .unwrap_or_else(|| panic!("no result was attributed to query `{query}`{run}"));
        assert_eq!(
            decode(attributed.need("stable_id", &run)),
            format!("echo:{query}"),
            "the result attributed to `{query}` must be the one that query produced{run}"
        );
    }
}

#[test]
fn dev_test_is_byte_for_byte_reproducible() {
    let scratch = Scratch::new("test-determinism");
    let plugin = plugin_dir(
        &scratch,
        "echo",
        "dev.crikey.m4.echo",
        "plugin:EchoPlugin",
        ECHO_SOURCE,
    );
    let args = [
        "dev", "test", "--plugin", &plugin, "--query", "foo", "--query", "bar",
    ];

    let first = succeed(&args);
    let second = succeed(&args);
    assert_eq!(
        first.stdout, second.stdout,
        "two runs of one invocation must print identical stdout, or the smoke cannot be diffed\
         \n--- first ---{first}\n--- second ---{second}"
    );
    assert_eq!(
        first.code, second.code,
        "the exit status must be reproducible too{first}"
    );
}

#[test]
fn dev_test_reports_a_plugin_that_raises_as_a_completed_run_that_found_a_fault() {
    let scratch = Scratch::new("test-raises");
    let plugin = plugin_dir(
        &scratch,
        "boom",
        "dev.crikey.m4.boom",
        "plugin:BoomPlugin",
        BOOM_SOURCE,
    );

    let run = run(&["dev", "test", "--plugin", &plugin, "--query", "foo"]);
    assert!(
        !run.stderr.contains("panicked at"),
        "a plugin raising must not crash the developer command{run}"
    );
    assert_eq!(
        run.code,
        Some(EX_FOUND_FAULT),
        "a completed run that found a fault exits {EX_FOUND_FAULT}, not {EX_USAGE} (that is for \
         a plugin that will not load){run}"
    );
    assert!(
        !run.stdout.trim().is_empty(),
        "a run that found a fault still learned something and must print it{run}"
    );

    let records = parse(&run);
    let summary = summary(&records, &run);
    assert_eq!(
        field(&summary, "state", &run),
        "failed",
        "a query whose callback raised reaches the terminal state `failed`{run}"
    );
    assert!(
        mentions(&records, "boom in suggest for foo"),
        "the run must report the message the plugin raised, not swallow it{run}"
    );
}

#[test]
fn dev_test_exercises_cancellation_and_reports_whether_the_plugin_cooperated() {
    let scratch = Scratch::new("test-cancel");
    let plugin = plugin_dir(
        &scratch,
        "cancel",
        "dev.crikey.m4.cancel",
        "plugin:CancelPlugin",
        COOPERATIVE_CANCEL_SOURCE,
    );

    let run = succeed(&["dev", "test", "--plugin", &plugin, "--query", "foo", "--cancel"]);
    let records = parse(&run);
    let summary = summary(&records, &run);

    assert_eq!(
        field(&summary, "state", &run),
        "cancelled",
        "a plugin that returns on cancellation yields the terminal state `cancelled`{run}"
    );
    assert_eq!(
        field(&summary, "cooperated", &run),
        "true",
        "a plugin that observed the cancellation and returned must be reported as cooperating{run}"
    );
}

#[test]
fn dev_test_uses_a_managed_dependency_from_the_offline_index() {
    // Acceptance §31.19: the plugin declares a dependency, the worker's import
    // path includes the materialised env, and the plugin imports and *uses* it.
    // The proof is the real interpreter importing the right version and its
    // output reaching a result — not merely that an env id was computed.
    let scratch = Scratch::new("test-managed-dep");
    let index = acme_index(&scratch, "index", "1.0.0");
    let dir = scratch.subdir("acme-plugin");
    write(
        &dir.join("crikey.toml"),
        &dep_plugin_manifest("dev.crikey.m4.acme", "plugin:AcmePlugin", "acme==1.0.0"),
    );
    write(&dir.join("plugin.py"), ACME_PLUGIN_SOURCE);
    let plugin = display(&dir);

    let run = succeed(&[
        "dev", "test", "--plugin", &plugin, "--index", &index, "--query", "world",
    ]);
    let records = parse(&run);
    let items = items(&records);
    assert_eq!(
        items.len(),
        1,
        "the plugin emits one result built from its dependency{run}"
    );
    assert_eq!(
        decode(items[0].need("label", &run)),
        "acme-1.0.0 greets world",
        "the declared dependency must be importable and executed inside the worker{run}"
    );
}

// ---------------------------------------------------------------------------
// `dev run` — one query end to end through a real worker
// ---------------------------------------------------------------------------

#[test]
fn dev_run_drives_one_query_end_to_end_and_prints_the_items() {
    let scratch = Scratch::new("run-one-query");
    let plugin = plugin_dir(
        &scratch,
        "echo",
        "dev.crikey.m4.echo",
        "plugin:EchoPlugin",
        ECHO_SOURCE,
    );

    let run = succeed(&["dev", "run", "--plugin", &plugin, "--query", "foo"]);
    let records = parse(&run);
    let items = items(&records);
    assert!(
        !items.is_empty(),
        "`dev run` must print the items the live worker produced{run}"
    );
    assert!(
        items
            .iter()
            .any(|item| decode(item.need("stable_id", &run)) == "echo:foo"),
        "the result driven end to end must be the one this query produced{run}"
    );
}

// ---------------------------------------------------------------------------
// Bad input is refused, never crashed on
// ---------------------------------------------------------------------------

#[test]
fn an_unusable_argument_list_is_refused_with_usage_status() {
    let scratch = Scratch::new("bad-args");
    let plugin = plugin_dir(
        &scratch,
        "echo",
        "dev.crikey.m4.echo",
        "plugin:EchoPlugin",
        ECHO_SOURCE,
    );
    let rejected: Vec<Vec<&str>> = vec![
        vec!["dev", "test"],
        vec!["dev", "test", "--plugin"],
        vec!["dev", "test", "--plugin", ""],
        vec!["dev", "test", "--plugin=", "--query", "foo"],
        vec!["dev", "test", "--plugin", &plugin, "--sideways"],
        vec!["dev", "run"],
        vec!["dev", "run", "--plugin"],
        vec!["dev", "run", "--plugin", ""],
        vec!["dev", "run", "--query", "foo"],
        vec!["dev", "test", "--plugin", "--query", "foo"],
        vec!["dev", "test", "--plugin", &plugin, "--query", "--unknown"],
        vec!["dev", "test", "--plugin", &plugin, "--plugin=second"],
        vec!["dev", "test", "--plugin", &plugin, "--help", "--query"],
    ];

    for args in rejected {
        let run = run(&args);
        assert_refused(&run);
    }
}

#[test]
fn a_plugin_directory_that_is_not_a_modern_plugin_is_refused_with_usage_status() {
    let scratch = Scratch::new("unloadable");

    // 1. A directory with no manifest at all is not a plugin.
    let bare = scratch.subdir("bare");
    // 2. A manifest whose entrypoint class does not exist cannot be loaded.
    let missing_class = plugin_dir(
        &scratch,
        "missing-class",
        "dev.crikey.m4.missing",
        "plugin:NoSuchClass",
        ECHO_SOURCE,
    );
    // 3. A plugin module that raises on import cannot be loaded.
    let unimportable = plugin_dir(
        &scratch,
        "unimportable",
        "dev.crikey.m4.unimportable",
        "plugin:Whatever",
        UNIMPORTABLE_SOURCE,
    );
    // 4. A path that does not exist at all.
    let absent = scratch.absent("no-such-plugin");

    for path in [&display(&bare), &missing_class, &unimportable, &absent] {
        for command in ["test", "run"] {
            let run = run(&["dev", command, "--plugin", path, "--query", "foo"]);
            assert_refused(&run);
        }
    }
}
