//! Red-first black-box tests for `crikey dev inspect-protocol` (spec 26.3,
//! 28; contract §5.1; acceptance §31.21, §31.29).
//!
//! These tests run the CLI binary and speak only its frozen key/value output
//! contract. The conformance binaries are built from the out-of-tree workspace
//! at test time, so the tests exercise the same process boundary a plugin
//! author and a CI job use. They intentionally contain no sleeps: cancellation
//! and the bounded fixture waits are driven by the command under test.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use crikey_native_protocol::message::Payload;
use crikey_native_protocol::Message;

/// The binary under test, as built by cargo for this integration target.
const CRIKEY: &str = env!("CARGO_BIN_EXE_crikey");

/// A completed, conformant inspection.
const EX_OK: i32 = 0;
/// A completed inspection that found a protocol non-conformance.
const EX_NOT_CONFORMANT: i32 = 1;
/// A usage error or a plugin that could not be launched at all.
const EX_USAGE: i32 = 64;
/// The Rust runtime's status for an unwound panic.
const PANIC_STATUS: i32 = 101;

/// Payload kinds emitted by the frozen protocol schema (contract §2.3).
const PAYLOAD_KINDS: [&str; 21] = [
    "handshake",
    "handshake_ack",
    "suggest",
    "results",
    "cancel",
    "shutdown",
    "catalog_request",
    "catalog_batch",
    "execute",
    "execute_result",
    "configuration",
    "event",
    "log",
    "health_check",
    "health_report",
    "error",
    "flow",
    "resource_request",
    "resource_response",
    "lifecycle",
    "lifecycle_ack",
];

/// Every ordinary echo run must report each of these checks exactly once.
const ECHO_CHECKS: [&str; 10] = [
    "handshake",
    "protocol-version",
    "session-token",
    "catalog-streaming",
    "suggest-streaming",
    "request-id-echo",
    "generation-echo",
    "credit-discipline",
    "frame-bounds",
    "shutdown",
];

/// One completed invocation, retained so assertion failures show all output.
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
    let owned = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    run_owned(&owned)
}

fn run_owned(args: &[String]) -> Run {
    let mut command = Command::new(CRIKEY);
    command.args(args);
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("could not execute `{CRIKEY}` with {args:?}: {error}"));

    Run {
        args: args.to_vec(),
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)
            .unwrap_or_else(|_| panic!("`crikey {args:?}` wrote non-UTF-8 to stdout")),
        stderr: String::from_utf8(output.stderr)
            .unwrap_or_else(|_| panic!("`crikey {args:?}` wrote non-UTF-8 to stderr")),
    }
}

fn inspect(
    plugin: &Path,
    mode: &str,
    transport: &str,
    query: Option<&str>,
    cancel: bool,
    trace: bool,
) -> Run {
    let mut args = vec![
        "dev".to_owned(),
        "inspect-protocol".to_owned(),
        "--plugin".to_owned(),
        display(plugin),
        "--transport".to_owned(),
        transport.to_owned(),
        "--env".to_owned(),
        format!("CRIKEY_CONFORMANCE_MODE={mode}"),
    ];
    if let Some(query) = query {
        args.push("--query".to_owned());
        args.push(query.to_owned());
    }
    if cancel {
        args.push("--cancel".to_owned());
    }
    if trace {
        args.push("--trace".to_owned());
    }
    run_owned(&args)
}

fn assert_no_panic(run: &Run) {
    assert_ne!(
        run.code,
        Some(PANIC_STATUS),
        "inspect-protocol must not unwind on a plugin or usage error{run}"
    );
    assert!(
        !run.stderr.contains("panicked at"),
        "inspect-protocol must not print a panic backtrace{run}"
    );
}

fn assert_completed(run: &Run, code: i32) {
    assert_no_panic(run);
    assert_eq!(run.code, Some(code), "unexpected inspect-protocol status{run}");
    assert!(
        !run.stdout.trim().is_empty(),
        "the inspection must print a report{run}"
    );
}

fn assert_refused(run: &Run) {
    assert_no_panic(run);
    assert_eq!(
        run.code,
        Some(EX_USAGE),
        "the invocation must be a usage error{run}"
    );
    assert!(
        run.stdout.trim().is_empty(),
        "a plugin that cannot be launched must not print a protocol report{run}"
    );
    assert!(
        !run.stderr.trim().is_empty(),
        "a usage error needs a diagnostic{run}"
    );
}

// ---------------------------------------------------------------------------
// Reading the frozen key=value output
// ---------------------------------------------------------------------------

/// One printed line, split into its whitespace-safe fields.
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
            .unwrap_or_else(|| panic!("line {} has no `{key}` field{run}", self.line))
    }

    fn is_detail(&self) -> bool {
        self.get("check").is_some() || self.get("item").is_some() || self.get("trace").is_some()
    }
}

/// Parses percent-encoded `key=value` lines exactly as a shell consumer would.
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
                        .unwrap_or_else(|| panic!("line {number}: `{token}` is not `key=value`{run}"));
                    assert!(!key.is_empty(), "line {number}: empty output key{run}");
                    assert!(
                        !value.contains('='),
                        "line {number}: bare `=` in `{token}`; values must use `%3D`{run}"
                    );
                    assert!(
                        seen.insert(key.to_owned()),
                        "line {number}: repeated key `{key}` makes the line ambiguous{run}"
                    );
                    (key.to_owned(), value.to_owned())
                })
                .collect::<Vec<_>>();
            assert!(!fields.is_empty(), "line {number}: no fields{run}");
            Record { line: number, fields }
        })
        .collect()
}

/// Returns summary fields and rejects a duplicated summary key.
fn summary(records: &[Record], run: &Run) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for record in records.iter().filter(|record| !record.is_detail()) {
        for (key, value) in &record.fields {
            assert!(
                fields.insert(key.clone(), value.clone()).is_none(),
                "line {}: summary key `{key}` was reported twice{run}",
                record.line
            );
        }
    }
    fields
}

fn field<'a>(summary: &'a BTreeMap<String, String>, key: &str, run: &Run) -> &'a str {
    summary
        .get(key)
        .unwrap_or_else(|| panic!("the report has no summary field `{key}`{run}"))
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

fn traces(records: &[Record]) -> Vec<&Record> {
    records
        .iter()
        .filter(|record| record.get("trace").is_some())
        .collect()
}

fn check_named<'a>(records: &'a [Record], name: &str, run: &Run) -> &'a Record {
    let mut matching = checks(records)
        .into_iter()
        .filter(|record| record.get("check") == Some(name));
    let check = matching
        .next()
        .unwrap_or_else(|| panic!("the report has no `{name}` check{run}"));
    assert!(
        matching.next().is_none(),
        "the `{name}` check was reported more than once{run}"
    );
    check
}

fn assert_checks(records: &[Record], expected: &[&str], result: &str, run: &Run) {
    let checks = checks(records);
    assert_eq!(
        checks.len(),
        expected.len(),
        "the report must contain exactly the frozen checks {expected:?}{run}"
    );
    for name in expected {
        let check = check_named(records, name, run);
        assert_eq!(
            check.need("result", run),
            result,
            "the `{name}` check has the wrong result{run}"
        );
        assert!(
            check.get("detail").is_some(),
            "the `{name}` check must carry its frozen detail field{run}"
        );
    }
}
fn assert_echo_checks_with_cancellation(records: &[Record], cancellation_result: &str, run: &Run) {
    assert_eq!(
        checks(records).len(),
        ECHO_CHECKS.len() + 1,
        "a cancelled run must report the ten base checks plus cancellation{run}"
    );
    for name in ECHO_CHECKS {
        let check = check_named(records, name, run);
        assert_eq!(
            check.need("result", run),
            "pass",
            "the `{name}` check failed{run}"
        );
        assert!(
            check.get("detail").is_some(),
            "the `{name}` check needs detail{run}"
        );
    }
    let cancellation = check_named(records, "cancellation", run);
    assert_eq!(
        cancellation.need("result", run),
        cancellation_result,
        "the cancellation check has the wrong result{run}"
    );
    assert!(
        cancellation.get("detail").is_some(),
        "cancellation needs detail{run}"
    );
}

/// Decodes uppercase percent escapes and rejects ambiguous or malformed ones.
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
                "`{value}` uses lowercase percent escapes"
            );
            let byte = u8::from_str_radix(hex, 16)
                .unwrap_or_else(|_| panic!("`{value}` contains invalid escape `%{hex}`"));
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| panic!("`{value}` is not UTF-8 after decoding"))
}

fn assert_echo_report(run: &Run, transport: &str, query_count: u64) -> Vec<Record> {
    assert_completed(run, EX_OK);
    let records = parse(run);
    let report = summary(&records, run);
    assert!(
        !field(&report, "plugin", run).is_empty(),
        "the handshake id is required{run}"
    );
    assert_eq!(
        field(&report, "protocol", run),
        "1",
        "protocol v1 is required{run}"
    );
    assert_eq!(
        field(&report, "transport", run),
        transport,
        "the selected transport is reported{run}"
    );
    assert_eq!(
        number(&report, "catalog_items", run),
        3,
        "echo has a three-item catalog{run}"
    );
    assert_eq!(
        number(&report, "queries", run),
        query_count,
        "query count must be reported{run}"
    );
    let result_count = number(&report, "results", run);
    assert!(result_count >= 1, "echo must return suggestion items{run}");
    assert_eq!(
        items(&records).len() as u64,
        result_count,
        "the results summary must count emitted item lines{run}"
    );
    assert_eq!(
        field(&report, "state", run),
        "final",
        "echo reaches final state{run}"
    );
    assert_eq!(
        field(&report, "verdict", run),
        "conformant",
        "echo is conformant{run}"
    );
    let capabilities = field(&report, "capabilities", run);
    let original = capabilities.split(',').collect::<Vec<_>>();
    assert!(
        !original.is_empty() && original.iter().all(|capability| !capability.is_empty()),
        "capabilities must be non-empty and comma-separated{run}"
    );
    let mut sorted = original.clone();
    sorted.sort_unstable();
    assert_eq!(original, sorted, "capabilities must be sorted{run}");
    assert_checks(&records, &ECHO_CHECKS, "pass", run);
    records
}

// ---------------------------------------------------------------------------
// Out-of-tree conformance fixture
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|candidate| candidate.join("compatibility").is_dir())
        .unwrap_or_else(|| panic!("could not find workspace root above CARGO_MANIFEST_DIR"))
        .to_path_buf()
}

/// Builds the out-of-tree conformance workspace once for this integration
/// binary and returns its well-behaved and intentionally misbehaving binaries.
fn conformance_binaries() -> (PathBuf, PathBuf) {
    static BINARIES: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BINARIES
        .get_or_init(|| {
            let root = workspace_root();
            let manifest = root.join("compatibility/native-conformance/Cargo.toml");
            assert!(
                manifest.is_file(),
                "the out-of-tree conformance manifest is missing at {}; tests do not skip",
                manifest.display()
            );
            let target = root.join("target/native-conformance");
            let output = Command::new("cargo")
                .current_dir(&root)
                .args([
                    "build",
                    "--manifest-path",
                    manifest
                        .to_str()
                        .unwrap_or_else(|| panic!("{} is not valid UTF-8", manifest.display())),
                    "--target-dir",
                    target
                        .to_str()
                        .unwrap_or_else(|| panic!("{} is not valid UTF-8", target.display())),
                    "--bins",
                ])
                .output()
                .unwrap_or_else(|error| panic!("could not build native conformance workspace: {error}"));
            assert!(
                output.status.success(),
                "building native conformance workspace failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let suffix = if cfg!(windows) { ".exe" } else { "" };
            let plugin = target
                .join("debug")
                .join(format!("crikey-conformance-plugin{suffix}"));
            let misbehaving = target
                .join("debug")
                .join(format!("crikey-misbehaving-plugin{suffix}"));
            assert!(
                plugin.is_file(),
                "conformance build did not produce {}",
                plugin.display()
            );
            assert!(
                misbehaving.is_file(),
                "conformance build did not produce {}",
                misbehaving.display()
            );
            (plugin, misbehaving)
        })
        .clone()
}

fn display(path: &Path) -> String {
    path.to_str()
        .unwrap_or_else(|| panic!("{} is not valid UTF-8", path.display()))
        .to_owned()
}

/// A private directory used only to prove a missing plugin is a usage error.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-m5-cli-{pid}-{ordinal}-{label}",
            pid = std::process::id(),
            ordinal = NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", path.display()));
        Self { path }
    }

    fn absent(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Command contract and healthy echo mode
// ---------------------------------------------------------------------------

#[test]
fn inspect_protocol_no_args_prints_usage_and_help_lists_every_flag() {
    let no_args = run(&["dev", "inspect-protocol"]);
    assert_no_panic(&no_args);
    assert_eq!(
        no_args.code,
        Some(EX_USAGE),
        "no arguments are a usage error{no_args}"
    );
    let usage = format!("{}{}", no_args.stdout, no_args.stderr).to_ascii_lowercase();
    assert!(
        usage.contains("usage") && usage.contains("inspect-protocol"),
        "no-args must print command usage{no_args}"
    );

    let help = run(&["dev", "inspect-protocol", "--help"]);
    assert_completed(&help, EX_OK);
    let help_text = format!("{}{}", help.stdout, help.stderr);
    for flag in [
        "--plugin",
        "--transport",
        "--query",
        "--cancel",
        "--trace",
        "--env",
    ] {
        assert!(
            help_text.contains(flag),
            "inspect-protocol help omits `{flag}`{help}"
        );
    }
    let (plugin, _) = conformance_binaries();
    let malformed_args = vec![
        "dev".to_owned(),
        "inspect-protocol".to_owned(),
        "--plugin".to_owned(),
        display(&plugin),
        "--env".to_owned(),
        "BROKEN".to_owned(),
    ];
    let malformed = run_owned(&malformed_args);
    assert_refused(&malformed);
}

#[test]
fn echo_mode_passes_every_frozen_check_once() {
    let (plugin, _) = conformance_binaries();
    let run = inspect(&plugin, "echo", "stdio", Some("foo"), false, false);
    let records = assert_echo_report(&run, "stdio", 1);
    let report = summary(&records, &run);
    assert!(
        number(&report, "results", &run) >= 1,
        "echo must return suggestion items{run}"
    );
    assert!(
        number(&report, "batches", &run) >= 3,
        "echo streams two partial batches and final{run}"
    );
    assert!(
        items(&records).iter().all(|item| {
            item.get("stable_id").is_some() && item.get("label").is_some() && item.get("target").is_some()
        }),
        "every emitted item must carry the frozen item fields{run}"
    );
}

#[test]
fn echo_inspection_is_byte_for_byte_reproducible() {
    let (plugin, _) = conformance_binaries();
    let first = inspect(&plugin, "echo", "stdio", Some("foo"), false, false);
    let second = inspect(&plugin, "echo", "stdio", Some("foo"), false, false);
    assert_completed(&first, EX_OK);
    assert_completed(&second, EX_OK);
    assert_eq!(
        first.code, second.code,
        "identical invocations need identical statuses{first}"
    );
    assert_eq!(
        first.stdout, second.stdout,
        "identical invocations need byte-identical stdout{first}"
    );
    assert_eq!(
        first.stderr, second.stderr,
        "identical invocations need byte-identical stderr{first}"
    );
}

#[test]
fn query_item_values_percent_encode_space_percent_and_equals() {
    let (plugin, _) = conformance_binaries();
    let query = "space % and =";
    let run = inspect(&plugin, "echo", "stdio", Some(query), false, false);
    let records = assert_echo_report(&run, "stdio", 1);
    let matching = items(&records)
        .into_iter()
        .find(|item| item.get("query").is_some())
        .unwrap_or_else(|| panic!("the item stream has no query field{run}"));
    assert_eq!(
        matching.need("query", &run),
        "space%20%25%20and%20%3D",
        "spaces, `%` and `=` must use the frozen percent encoding{run}"
    );
    assert_eq!(decode(matching.need("query", &run)), query);
}
#[test]
fn cancellation_check_distinguishes_cooperative_and_ignoring_plugins() {
    let (plugin, _) = conformance_binaries();

    let cooperative = inspect(&plugin, "slow:500", "stdio", Some("foo"), true, false);
    assert_completed(&cooperative, EX_OK);
    let cooperative_records = parse(&cooperative);
    let cooperative_report = summary(&cooperative_records, &cooperative);
    assert_eq!(field(&cooperative_report, "state", &cooperative), "cancelled");
    assert_eq!(field(&cooperative_report, "cooperated", &cooperative), "true");
    assert_eq!(field(&cooperative_report, "verdict", &cooperative), "conformant");
    assert_echo_checks_with_cancellation(&cooperative_records, "pass", &cooperative);

    let ignoring = inspect(&plugin, "ignore-cancel:500", "stdio", Some("foo"), true, false);
    assert_completed(&ignoring, EX_NOT_CONFORMANT);
    let ignoring_records = parse(&ignoring);
    let ignoring_report = summary(&ignoring_records, &ignoring);
    assert_eq!(field(&ignoring_report, "cooperated", &ignoring), "false");
    assert_eq!(field(&ignoring_report, "verdict", &ignoring), "non-conformant");
    assert_echo_checks_with_cancellation(&ignoring_records, "fail", &ignoring);
}

// ---------------------------------------------------------------------------
// Diagnostics, traces and transports
// ---------------------------------------------------------------------------

#[test]
fn misbehaving_fixtures_report_named_non_conformance_instead_of_crashing() {
    let (_, plugin) = conformance_binaries();
    for (mode, check) in [
        ("bad-version:2", "protocol-version"),
        ("flood", "credit-discipline"),
        ("oversized", "frame-bounds"),
    ] {
        let run = inspect(&plugin, mode, "stdio", Some("foo"), false, false);
        assert_completed(&run, EX_NOT_CONFORMANT);
        let records = parse(&run);
        let report = summary(&records, &run);
        assert_eq!(
            field(&report, "verdict", &run),
            "non-conformant",
            "{mode} verdict{run}"
        );
        assert_eq!(
            check_named(&records, check, &run).need("result", &run),
            "fail",
            "{mode} must fail its named defensive check{run}"
        );
    }
}

#[test]
fn an_unlaunchable_plugin_is_a_usage_error() {
    let scratch = Scratch::new("missing-plugin");
    let absent = scratch.absent("does-not-exist");
    let run = inspect(&absent, "echo", "stdio", Some("foo"), false, false);
    assert_refused(&run);
}

#[test]
fn trace_lines_precede_and_do_not_change_the_summary() {
    let (plugin, _) = conformance_binaries();
    let ordinary = inspect(&plugin, "echo", "stdio", Some("foo"), false, false);
    let traced = inspect(&plugin, "echo", "stdio", Some("foo"), false, true);
    assert_completed(&ordinary, EX_OK);
    assert_completed(&traced, EX_OK);
    let ordinary_records = parse(&ordinary);
    let traced_records = parse(&traced);
    let ordinary_summary = summary(&ordinary_records, &ordinary);
    let traced_summary = summary(&traced_records, &traced);
    assert_eq!(
        traced_summary, ordinary_summary,
        "trace must not alter summary keys or values{traced}"
    );

    let trace_records = traces(&traced_records);
    assert!(
        !trace_records.is_empty(),
        "--trace must print protocol messages{traced}"
    );
    assert!(
        traced_records
            .first()
            .and_then(|record| record.get("trace"))
            .is_some(),
        "trace messages must precede the summary{traced}"
    );
    let handshake = trace_records
        .iter()
        .find(|record| record.get("kind") == Some("handshake"))
        .unwrap_or_else(|| panic!("trace has no observed plugin handshake message{traced}"));
    assert_eq!(handshake.get("trace"), Some("plugin->host"));
    assert_eq!(handshake.get("request"), Some("0"));
    assert!(
        trace_records
            .iter()
            .all(|record| !(record.get("trace") == Some("host->plugin")
                && record.get("kind") == Some("handshake"))),
        "trace must not invent a host handshake envelope{traced}"
    );
    assert!(
        trace_records
            .iter()
            .all(|record| { !matches!(record.get("kind"), Some("lifecycle") | Some("lifecycle_ack")) }),
        "trace must not invent lifecycle envelopes{traced}"
    );
    for trace in trace_records {
        let kind = trace.need("kind", &traced);
        assert!(
            PAYLOAD_KINDS.contains(&kind),
            "trace kind `{kind}` is not a frozen Payload::kind string{traced}"
        );
        assert!(
            trace.get("request").is_some(),
            "trace request id is required{traced}"
        );
    }
}
fn empty_message<M: Message>() -> M {
    M::decode(&[]).expect("an empty message is a valid default")
}

#[test]
fn every_payload_kind_matches_its_frozen_proto_field_name() {
    let cases = [
        ("handshake", Payload::Handshake(empty_message())),
        ("handshake_ack", Payload::HandshakeAck(empty_message())),
        ("suggest", Payload::Suggest(empty_message())),
        ("results", Payload::Results(empty_message())),
        ("cancel", Payload::Cancel(empty_message())),
        ("shutdown", Payload::Shutdown(empty_message())),
        ("catalog_request", Payload::CatalogRequest(empty_message())),
        ("catalog_batch", Payload::CatalogBatch(empty_message())),
        ("execute", Payload::Execute(empty_message())),
        ("execute_result", Payload::ExecuteResult(empty_message())),
        ("configuration", Payload::Configuration(empty_message())),
        ("event", Payload::Event(empty_message())),
        ("log", Payload::Log(empty_message())),
        ("health_check", Payload::HealthCheck(empty_message())),
        ("health_report", Payload::HealthReport(empty_message())),
        ("error", Payload::Error(empty_message())),
        ("flow", Payload::Flow(empty_message())),
        ("resource_request", Payload::ResourceRequest(empty_message())),
        ("resource_response", Payload::ResourceResponse(empty_message())),
        ("lifecycle", Payload::Lifecycle(empty_message())),
        ("lifecycle_ack", Payload::LifecycleAck(empty_message())),
    ];

    for (expected, payload) in cases {
        assert_eq!(payload.kind(), expected);
    }
}

#[cfg(unix)]
#[test]
fn unix_transport_reaches_the_same_conformant_verdict() {
    let (plugin, _) = conformance_binaries();
    let run = inspect(&plugin, "echo", "unix", Some("foo"), false, false);
    let _ = assert_echo_report(&run, "unix", 1);
}
