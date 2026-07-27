//! Black-box tests for `crikey dev trace-query` and `crikey dev simulate-typing`
//! (spec 26.3, 26.4, 28; roadmap M2 / spec 30 Phase 2).
//!
//! These drive the built binary as a user does, over a named deterministic
//! fixture, and read only what it prints. Nothing here reaches into the
//! workspace's library types on purpose: the developer tooling is a contract
//! with whoever is debugging a scheduling bug at 2am with a shell, and that
//! contract is exit status plus stdout, not the shape of a Rust enum.
//!
//! # The output contract these tests hold the commands to
//!
//! Both commands print lines of whitespace-separated `key=value` tokens, the
//! same shape `crikey dev benchmark` already uses, so that `cut`, `grep` and
//! `sort` are a complete reader and no field ever needs quoting.
//!
//! * A line whose first token is `event=<category>` is one *trace event*.
//!   Categories include the snake-cased scheduler events of spec 26.4, bounded
//!   intake admissions/rejections/backpressure decisions, and the frames that
//!   were actually published. Every event carries `at_ms` (virtual
//!   milliseconds since the first keystroke) and `generation`.
//! * Every other non-blank line contributes `key=value` pairs to the run
//!   *summary*: the totals a stress run is judged by.
//!
//! `trace-query` prints the events and the summary. `simulate-typing` is the
//! same run reported as the summary alone, which is why one test asserts the
//! two agree field for field: a trace and a stress verdict that can disagree
//! describe two different runs, and neither is then evidence about the other.
//!
//! # Why there is no clock here
//!
//! Every timestamp in the output is virtual: chosen by the fixture and the
//! `--interval-ms` argument, never sampled from the host. That is what lets
//! these tests demand byte-identical output from two consecutive runs. A
//! developer tool whose trace changes when the machine is busy cannot be used
//! to diagnose a scheduling bug, because the tool's own noise is
//! indistinguishable from the bug's.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::process::Command;

/// The binary under test, as built by cargo for this integration target.
const CRIKEY: &str = env!("CARGO_BIN_EXE_crikey");

/// `EX_USAGE`: the caller's fault, and the only failure a bad argument list
/// may produce.
const EX_USAGE: i32 = 64;
/// `EX_UNAVAILABLE`: what `crikey dev` answers for a subcommand it advertises
/// but has not built. An implemented command must never answer with it.
const EX_UNAVAILABLE: i32 = 69;
/// The exit status the Rust runtime uses for an unwound panic.
const PANIC_STATUS: i32 = 101;

/// Fixtures both commands must offer, and what each is for.
///
/// Named rather than assembled from flags because a scheduling report only
/// compares against another report of the same workload, and a fixture is the
/// only way two developers on two machines can be sure they ran one.
const REQUIRED_FIXTURES: [&str; 4] = [
    // A single modern plugin with a debounce interval and a maximum wait.
    "modern-debounce",
    // A single `legacy-strict` plugin: prompt dispatch, never time debounced.
    "legacy-strict",
    // A fast plugin and a slow plugin answering the same generations.
    "slow-and-fast",
    // All of the above at once: the M2 rapid-typing stress workload.
    "rapid-typing",
];

/// Trace event categories of spec 26.4 that the stress fixture must produce.
const REQUIRED_EVENTS: [&str; 19] = [
    "keystroke",
    "debounce",
    "legacy_dispatch",
    "dispatched",
    "cancelled",
    "first_result",
    "final_result",
    "result_batch",
    "stale_result_rejected",
    "ranking",
    "presentation",
    "request_dropped",
    "frame",
    "result_queue_admitted",
    "result_queue_rejected",
    "result_queue_evicted",
    "result_queue_merged",
    "producer_paused",
    "producer_resumed",
];

/// Summary fields every successful run of either command must report.
const REQUIRED_SUMMARY: [&str; 48] = [
    "fixture",
    "keystrokes",
    "generations",
    "plugins",
    "workload_keystroke_limit",
    "request_queue_capacity",
    "peak_queue_depth",
    "result_queue_capacity",
    "peak_result_queue_depth",
    "request_queue_overflow_policy",
    "result_queue_overflow_policy",
    "max_pending_per_plugin",
    "coalesced_requests",
    "dropped_obsolete_requests",
    "rejected_plugin_queue_full",
    "rejected_global_queue_full",
    "discarded_requests",
    "dispatched_requests",
    "cancelled_requests",
    "rejected_stale_results",
    "stale_results_displayed",
    "cross_generation_reorderings",
    "presented_frames",
    "presented_items",
    "first_result_latency_ms",
    "final_result_latency_ms",
    "trace_capacity",
    "trace_events_dropped",
    "intake_events_dropped",
    "pipeline_errors_dropped",
    "trace_truncated",
    "result_batches_admitted",
    "result_batches_merged",
    "result_batches_rejected",
    "result_merge_rejected",
    "result_batches_evicted",
    "result_obsolete_batches_dropped",
    "result_producer_pauses",
    "result_producer_resumes",
    "effective_legacy_queue_policy",
    "effective_legacy_queue_capacity",
    "effective_legacy_max_concurrent_requests",
    "legacy_coalesced_requests",
    "legacy_dropped_obsolete_requests",
    "legacy_rejected_queue_full",
    "legacy_dispatched_requests",
    "legacy_cancelled_requests",
    "legacy_rejected_stale_results",
];

/// Named request-queue policies: what the scheduler does with an arrival it
/// has no room for (spec 12.4). A queue whose overflow behaviour is unnamed is
/// not a bounded queue, it is a queue nobody has decided about yet.
const REQUEST_QUEUE_POLICIES: [&str; 3] = ["replace-oldest", "reject-newest", "drop-oldest"];

/// Named result-queue policies at the aggregator boundary (spec 12.3, 12.4).
const RESULT_QUEUE_POLICIES: [&str; 4] = [
    "reject-low-priority",
    "pause-producer",
    "replace-oldest",
    "disconnect",
];

/// Batch completion states of spec 12.5.
const COMPLETIONS: [&str; 4] = ["partial", "final", "cancelled", "failed"];

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
    assert_eq!(run.code, Some(0), "expected success{run}");
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

    fn event(&self) -> Option<&str> {
        self.get("event")
    }

    fn need(&self, key: &str, run: &Run) -> &str {
        self.get(key).unwrap_or_else(|| {
            panic!(
                "line {line}: `{event}` event has no `{key}` field{run}",
                line = self.line,
                event = self.event().unwrap_or("<summary>"),
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

    fn at_ms(&self, run: &Run) -> u64 {
        self.number("at_ms", run)
    }

    fn generation(&self, run: &Run) -> u64 {
        self.number("generation", run)
    }
}

/// Splits stdout into records, requiring every token to be `key=value`.
///
/// Strict on purpose: one unsplittable token is enough to make the whole
/// stream unreadable by the shell pipeline the format exists for, and a
/// developer discovers that at the moment they need the trace.
fn parse(run: &Run) -> Vec<Record> {
    run.stdout
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let number = index + 1;
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
                    (key.to_owned(), value.to_owned())
                })
                .collect::<Vec<_>>();
            assert!(!fields.is_empty(), "line {number}: no fields{run}");
            Record { line: number, fields }
        })
        .collect()
}

fn events<'a>(records: &'a [Record], category: &str) -> Vec<&'a Record> {
    records
        .iter()
        .filter(|record| record.event() == Some(category))
        .collect()
}

fn all_events(records: &[Record]) -> Vec<&Record> {
    records.iter().filter(|record| record.event().is_some()).collect()
}

/// Every summary field of the run, refusing a field reported twice.
fn summary(records: &[Record], run: &Run) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for record in records.iter().filter(|record| record.event().is_none()) {
        for (key, value) in &record.fields {
            let previous = fields.insert(key.clone(), value.clone());
            assert!(
                previous.is_none(),
                "line {line}: summary reports `{key}` twice; a reader cannot tell which run it \
                 describes{run}",
                line = record.line,
            );
        }
    }
    fields
}

fn summary_field<'a>(summary: &'a BTreeMap<String, String>, key: &str, run: &Run) -> &'a str {
    summary
        .get(key)
        .unwrap_or_else(|| panic!("the run summary has no `{key}` field{run}"))
        .as_str()
}

fn summary_number(summary: &BTreeMap<String, String>, key: &str, run: &Run) -> u64 {
    let raw = summary_field(summary, key, run);
    raw.parse()
        .unwrap_or_else(|_| panic!("summary `{key}={raw}` is not a whole number{run}"))
}

/// Runs a command and returns its parsed output alongside the run itself.
fn traced(args: &[&str]) -> (Run, Vec<Record>) {
    let run = succeed(args);
    let records = parse(&run);
    (run, records)
}

/// A query of the requested length whose every keystroke changes the text, so
/// each one is genuinely a new generation rather than a repeated character the
/// scheduler is free to ignore.
fn typing_of_length(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    (0..length)
        .map(|index| char::from(ALPHABET[index % ALPHABET.len()]))
        .collect()
}

// ---------------------------------------------------------------------------
// The commands exist and describe themselves
// ---------------------------------------------------------------------------

#[test]
fn both_developer_commands_are_implemented_rather_than_advertised() {
    for command in ["trace-query", "simulate-typing"] {
        let run = run(&["dev", command, "--list-fixtures"]);
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
    }
}

#[test]
fn help_explains_the_command_without_running_a_trace() {
    for command in ["trace-query", "simulate-typing"] {
        for flag in ["-h", "--help"] {
            let run = succeed(&["dev", command, flag]);
            let records = parse(&run);
            assert!(
                all_events(&records).is_empty(),
                "`dev {command} {flag}` must explain the command and trace nothing{run}"
            );
        }
    }
}

#[test]
fn both_commands_offer_every_fixture_the_m2_contract_needs() {
    for command in ["trace-query", "simulate-typing"] {
        let (run, records) = traced(&["dev", command, "--list-fixtures"]);

        let offered: BTreeSet<&str> = records
            .iter()
            .filter_map(|record| record.get("fixture"))
            .collect();
        assert!(
            !offered.is_empty(),
            "`dev {command} --list-fixtures` must print `fixture=<name>` lines{run}"
        );

        for required in REQUIRED_FIXTURES {
            assert!(
                offered.contains(required),
                "`dev {command}` does not offer the `{required}` fixture{run}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Determinism: the whole point of a virtual clock
// ---------------------------------------------------------------------------

#[test]
fn repeating_an_invocation_reproduces_it_byte_for_byte() {
    for args in [
        ["dev", "trace-query", "--fixture", "rapid-typing"],
        ["dev", "simulate-typing", "--fixture", "rapid-typing"],
    ] {
        let first = succeed(&args);
        let second = succeed(&args);

        assert_eq!(
            first.stdout, second.stdout,
            "two runs of the same fixture disagree, so the trace is sampling a real clock \
             somewhere{first}{second}"
        );
        assert_eq!(first.stderr, second.stderr, "{first}{second}");
        assert_eq!(first.code, second.code, "{first}{second}");
    }
}

#[test]
fn keystroke_timestamps_are_the_ones_the_arguments_asked_for() {
    let (run, records) = traced(&[
        "dev",
        "trace-query",
        "--fixture",
        "modern-debounce",
        "--query",
        "fire",
        "--interval-ms",
        "25",
    ]);

    let keystrokes = events(&records, "keystroke");
    assert_eq!(
        keystrokes.len(),
        4,
        "typing `fire` is four keystrokes, one per character{run}"
    );

    for (index, keystroke) in keystrokes.iter().enumerate() {
        let expected_at = 25 * index as u64;
        assert_eq!(
            keystroke.at_ms(&run),
            expected_at,
            "keystroke {index} should land at {expected_at}ms of virtual time{run}"
        );
        assert_eq!(
            keystroke.number("query_length", &run),
            index as u64 + 1,
            "keystroke {index} should have grown the query to {} characters{run}",
            index + 1
        );
    }

    let generations: Vec<u64> = keystrokes
        .iter()
        .map(|keystroke| keystroke.generation(&run))
        .collect();
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "query generations must increase strictly with each keystroke, got {generations:?}{run}"
    );
}

// ---------------------------------------------------------------------------
// The trace itself (spec 26.4)
// ---------------------------------------------------------------------------

#[test]
fn the_stress_trace_contains_every_query_trace_category() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);

    for category in REQUIRED_EVENTS {
        assert!(
            !events(&records, category).is_empty(),
            "the rapid-typing trace never reports `{category}`, so a developer cannot see that \
             part of the query's life{run}"
        );
    }

    let summary = summary(&records, &run);
    for field in REQUIRED_SUMMARY {
        let value = summary_field(&summary, field, &run);
        assert!(!value.is_empty(), "summary field `{field}` is empty{run}");
    }
}

#[test]
fn every_trace_event_is_placed_in_virtual_time_and_in_a_generation() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);
    let trace = all_events(&records);
    assert!(trace.len() > 1, "a trace of one event is not a trace{run}");

    let mut previous_at = 0;
    for event in &trace {
        let at = event.at_ms(&run);
        assert!(
            at >= previous_at,
            "line {line}: the trace goes backwards, from {previous_at}ms to {at}ms; a log that is \
             not in time order cannot be read as one{run}",
            line = event.line,
        );
        previous_at = at;

        let generation = event.generation(&run);
        assert!(
            generation > 0,
            "line {line}: generation 0 is not a query{run}",
            line = event.line,
        );
    }

    assert_eq!(
        trace[0].event(),
        Some("keystroke"),
        "a query trace starts with the keystroke that began the query{run}"
    );
    assert_eq!(
        trace[0].at_ms(&run),
        0,
        "the first keystroke is the trace's epoch{run}"
    );
}

#[test]
fn plugin_scoped_events_name_the_plugin_they_concern() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);

    let mut named = BTreeSet::new();
    for category in [
        "debounce",
        "legacy_dispatch",
        "dispatched",
        "cancelled",
        "first_result",
        "final_result",
        "result_batch",
        "stale_result_rejected",
    ] {
        for event in events(&records, category) {
            let plugin = event.need("plugin", &run);
            assert!(
                !plugin.is_empty(),
                "line {line}: `{category}` names an empty plugin{run}",
                line = event.line,
            );
            named.insert(plugin.to_owned());
        }
    }

    let summary = summary(&records, &run);
    assert_eq!(
        named.len() as u64,
        summary_number(&summary, "plugins", &run),
        "the trace mentions {named:?}, which is not the plugin count the summary reports{run}"
    );
}

#[test]
fn result_batches_report_a_size_and_a_completion_state() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);

    let batches = events(&records, "result_batch");
    assert!(!batches.is_empty(), "{run}");

    let mut saw_final = false;
    for batch in batches {
        // A size, not an item list: the trace is a record of scheduling, and a
        // batch of 400 items must not turn the trace into the results.
        batch.number("items", &run);

        let completion = batch.need("completion", &run);
        assert!(
            COMPLETIONS.contains(&completion),
            "line {line}: `completion={completion}` is not one of {COMPLETIONS:?}{run}",
            line = batch.line,
        );
        saw_final |= completion == "final";
    }
    assert!(
        saw_final,
        "no batch is ever final, so nothing in the trace says a query finished{run}"
    );
}

#[test]
fn first_and_final_result_latencies_agree_with_the_events_that_report_them() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);
    let summary = summary(&records, &run);

    for (category, field) in [
        ("first_result", "first_result_latency_ms"),
        ("final_result", "final_result_latency_ms"),
    ] {
        let latencies: Vec<u64> = events(&records, category)
            .iter()
            .map(|event| event.number("latency_ms", &run))
            .collect();
        assert!(
            !latencies.is_empty(),
            "no `{category}` event, so `{field}` describes nothing{run}"
        );

        let reported = summary_number(&summary, field, &run);
        assert!(
            latencies.contains(&reported),
            "summary `{field}={reported}` matches none of the `{category}` latencies \
             {latencies:?}{run}"
        );
    }

    assert!(
        summary_number(&summary, "final_result_latency_ms", &run)
            >= summary_number(&summary, "first_result_latency_ms", &run),
        "the final result cannot arrive before the first one{run}"
    );
}

// ---------------------------------------------------------------------------
// Debounce, coalescing and the maximum wait (spec 8)
// ---------------------------------------------------------------------------

#[test]
fn only_the_newest_undispatched_query_survives_rapid_typing() {
    let (probe, probe_records) = traced(&["dev", "trace-query", "--fixture", "modern-debounce"]);
    let debounce_ms = summary_number(&summary(&probe_records, &probe), "debounce_ms", &probe);
    assert!(
        debounce_ms > 1,
        "the modern-debounce fixture must actually debounce{probe}"
    );

    // Type faster than the quiet period, so every keystroke lands on a plugin
    // that is already holding an undispatched query.
    let interval = (debounce_ms / 2).max(1).to_string();
    let query = typing_of_length(24);
    let (run, records) = traced(&[
        "dev",
        "trace-query",
        "--fixture",
        "modern-debounce",
        "--query",
        &query,
        "--interval-ms",
        &interval,
    ]);

    let keystrokes = events(&records, "keystroke").len();
    let dispatched = events(&records, "dispatched");
    assert!(
        dispatched.len() < keystrokes,
        "{count} dispatches for {keystrokes} keystrokes: nothing was coalesced{run}",
        count = dispatched.len(),
    );

    let generations: Vec<u64> = dispatched.iter().map(|event| event.generation(&run)).collect();
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "the plugin was sent generations {generations:?}: an older query was dispatched after a \
         newer one{run}"
    );

    let summary = summary(&records, &run);
    assert_eq!(
        summary_number(&summary, "max_pending_per_plugin", &run),
        1,
        "more than one undispatched query was retained for a plugin{run}"
    );
    assert!(
        summary_number(&summary, "coalesced_requests", &run) > 0,
        "typing {keystrokes} characters inside the debounce window coalesced nothing{run}"
    );
}

#[test]
fn continuous_typing_still_dispatches_within_the_maximum_wait() {
    let (probe, probe_records) = traced(&["dev", "trace-query", "--fixture", "modern-debounce"]);
    let probe_summary = summary(&probe_records, &probe);
    let debounce_ms = summary_number(&probe_summary, "debounce_ms", &probe);
    let maximum_wait_ms = summary_number(&probe_summary, "maximum_wait_ms", &probe);
    assert!(
        maximum_wait_ms > debounce_ms,
        "a maximum wait no longer than the debounce interval postpones nothing{probe}"
    );
    assert!(
        maximum_wait_ms <= 5_000,
        "a {maximum_wait_ms}ms ceiling on postponement is not a responsiveness guarantee{probe}"
    );

    // Never pause: without a maximum wait, a trailing-edge debouncer would sit
    // on this query until the typing stopped.
    let interval = (debounce_ms / 2).max(1);
    let keystrokes = (maximum_wait_ms / interval + 4) as usize;
    let query = typing_of_length(keystrokes);
    let (run, records) = traced(&[
        "dev",
        "trace-query",
        "--fixture",
        "modern-debounce",
        "--query",
        &query,
        "--interval-ms",
        &interval.to_string(),
    ]);

    let decisions: Vec<&str> = events(&records, "debounce")
        .iter()
        .map(|event| event.need("decision", &run))
        .collect();
    assert!(
        decisions.contains(&"maximum-wait"),
        "typing without pause for {maximum_wait_ms}ms produced decisions {decisions:?} and never \
         hit the maximum wait{run}"
    );

    let dispatch_times: Vec<u64> = events(&records, "dispatched")
        .iter()
        .map(|event| event.at_ms(&run))
        .collect();
    assert!(
        dispatch_times.len() >= 2,
        "continuous typing dispatched {dispatch_times:?}: the plugin was starved{run}"
    );
    let mut previous = 0;
    for at in &dispatch_times {
        let gap = at - previous;
        assert!(
            gap <= maximum_wait_ms,
            "{gap}ms passed with nothing dispatched, past the {maximum_wait_ms}ms ceiling{run}"
        );
        previous = *at;
    }

    let last_keystroke = events(&records, "keystroke")
        .last()
        .expect("the fixture typed something")
        .generation(&run);
    let last_dispatched = events(&records, "dispatched")
        .last()
        .expect("something was dispatched")
        .generation(&run);
    assert_eq!(
        last_dispatched, last_keystroke,
        "the query the user actually finished typing was never sent{run}"
    );
}

// ---------------------------------------------------------------------------
// legacy-strict (spec 8.4, 13.4)
// ---------------------------------------------------------------------------

#[test]
fn legacy_strict_dispatches_promptly_and_is_never_time_debounced() {
    let (run, records) = traced(&[
        "dev",
        "trace-query",
        "--fixture",
        "legacy-strict",
        "--query",
        "fire",
        "--interval-ms",
        "10",
    ]);

    let deferrals: Vec<&Record> = events(&records, "debounce")
        .into_iter()
        .filter(|event| {
            matches!(
                event.need("decision", &run),
                "deferred" | "trailing-edge" | "maximum-wait"
            )
        })
        .collect();
    assert!(
        deferrals.is_empty(),
        "a legacy-strict plugin was time debounced on lines {lines:?}{run}",
        lines = deferrals.iter().map(|event| event.line).collect::<Vec<_>>(),
    );

    let legacy = events(&records, "legacy_dispatch");
    assert!(
        !legacy.is_empty(),
        "the legacy fixture reported no dispatch decisions at all{run}"
    );

    let first = legacy[0];
    assert_eq!(
        first.at_ms(&run),
        0,
        "the first keystroke found the plugin idle, so it should have been sent at once{run}"
    );
    assert_eq!(first.need("decision", &run), "now", "{run}");

    let decisions: Vec<&str> = legacy.iter().map(|event| event.need("decision", &run)).collect();
    assert!(
        decisions.contains(&"queued-behind-running"),
        "typing while the plugin was busy produced {decisions:?}: nothing was ever queued behind \
         a running callback{run}"
    );

    // Serialization: a second dispatch may not happen before the first one is
    // accounted for by a result batch or a cancellation.
    let mut in_flight = 0i64;
    for event in all_events(&records) {
        match event.event() {
            Some("dispatched") => {
                in_flight += 1;
                assert!(
                    in_flight <= 1,
                    "line {line}: two legacy callbacks are running at once{run}",
                    line = event.line,
                );
            }
            Some("final_result" | "cancelled") => in_flight = (in_flight - 1).max(0),
            _ => {}
        }
    }

    let summary = summary(&records, &run);
    assert_eq!(
        summary_field(&summary, "effective_legacy_queue_policy", &run),
        "replace-oldest",
        "legacy-strict must report the canonical policy the scheduler actually applies{run}"
    );
    assert_eq!(
        summary_number(&summary, "effective_legacy_queue_capacity", &run),
        1,
        "{run}"
    );
    assert_eq!(
        summary_number(&summary, "effective_legacy_max_concurrent_requests", &run),
        1,
        "{run}"
    );
    assert!(
        summary_number(&summary, "legacy_coalesced_requests", &run) > 0,
        "rapid typing at a busy legacy plugin replaced no obsolete pending request{run}"
    );
    assert_eq!(
        summary_number(&summary, "legacy_dropped_obsolete_requests", &run),
        0,
        "replace-oldest work was mislabeled as drop-oldest work{run}"
    );
    assert_eq!(
        summary_number(&summary, "legacy_coalesced_requests", &run),
        summary_number(&summary, "coalesced_requests", &run),
        "the single legacy plugin's observed counter disagrees with the aggregate{run}"
    );
}

// ---------------------------------------------------------------------------
// Cancellation and stale results (spec 31.6, 31.7)
// ---------------------------------------------------------------------------

#[test]
fn obsolete_in_flight_work_is_cancelled_and_its_results_rejected() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);

    let newest = events(&records, "keystroke")
        .last()
        .expect("the fixture typed something")
        .generation(&run);

    let cancelled = events(&records, "cancelled");
    assert!(
        !cancelled.is_empty(),
        "rapid typing cancelled nothing in flight{run}"
    );
    for event in &cancelled {
        assert!(
            event.generation(&run) < newest,
            "line {line}: the newest generation was cancelled{run}",
            line = event.line,
        );
        let reason = event.need("reason", &run);
        assert!(
            !reason.is_empty(),
            "line {line}: a cancellation with no stated reason is not a diagnostic{run}",
            line = event.line,
        );
    }

    let rejected = events(&records, "stale_result_rejected");
    assert!(
        !rejected.is_empty(),
        "no plugin ever answered late, so `stale_results_displayed=0` would prove nothing{run}"
    );

    let summary = summary(&records, &run);
    assert_eq!(
        summary_number(&summary, "rejected_stale_results", &run),
        rejected.len() as u64,
        "the summary and the trace disagree about how many stale answers arrived{run}"
    );
    assert_eq!(
        summary_number(&summary, "stale_results_displayed", &run),
        0,
        "a result from a superseded generation reached the user{run}"
    );
    assert_eq!(
        summary_number(&summary, "cancelled_requests", &run),
        cancelled.len() as u64,
        "{run}"
    );
}

#[test]
fn what_is_presented_never_moves_back_to_an_older_generation() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);

    let presentations = events(&records, "presentation");
    assert!(
        presentations.len() >= 2,
        "a single presentation cannot show whether the list ever went backwards{run}"
    );

    let mut highest = 0;
    for event in &presentations {
        let generation = event.generation(&run);
        assert!(
            generation >= highest,
            "line {line}: presented generation {generation} after {highest}{run}",
            line = event.line,
        );
        highest = generation;
        event.number("visible_items", &run);
    }

    // A new generation is presented empty immediately to clear old rows; once
    // it carries items, it must be one a plugin was actually asked about.
    let dispatched: BTreeSet<(u64, u64)> = events(&records, "dispatched")
        .iter()
        .map(|event| (event.generation(&run), event.at_ms(&run)))
        .collect();
    for event in &presentations {
        if event.number("visible_items", &run) == 0 {
            continue;
        }
        let generation = event.generation(&run);
        let at = event.at_ms(&run);
        assert!(
            dispatched
                .iter()
                .any(
                    |(dispatched_generation, dispatched_at)| *dispatched_generation == generation
                        && *dispatched_at <= at
                ),
            "line {line}: populated generation {generation} was presented at {at}ms but never \
             dispatched before then{run}",
            line = event.line,
        );
    }

    assert_eq!(
        summary_number(&summary(&records, &run), "cross_generation_reorderings", &run),
        0,
        "{run}"
    );

    // Presentation is what the user sees; ranking is the decision behind it.
    // A list shown for a generation nothing was ever ranked in is a list whose
    // order no part of the trace explains.
    let rankings: Vec<(u64, u64)> = events(&records, "ranking")
        .iter()
        .map(|event| (event.generation(&run), event.at_ms(&run)))
        .collect();
    for event in &presentations {
        let generation = event.generation(&run);
        let at = event.at_ms(&run);
        assert!(
            rankings
                .iter()
                .any(|(ranked_generation, ranked_at)| *ranked_generation == generation && *ranked_at <= at),
            "line {line}: generation {generation} was presented at {at}ms without a ranking \
             update behind it{run}",
            line = event.line,
        );
    }
}

// ---------------------------------------------------------------------------
// A slow plugin never delays a fast one (spec 31.8)
// ---------------------------------------------------------------------------

#[test]
fn the_fast_plugin_is_shown_before_the_slow_one_answers() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "slow-and-fast"]);

    let mut first_result_by_plugin: BTreeMap<&str, u64> = BTreeMap::new();
    for event in events(&records, "first_result") {
        first_result_by_plugin
            .entry(event.need("plugin", &run))
            .or_insert_with(|| event.at_ms(&run));
    }
    assert!(
        first_result_by_plugin.len() >= 2,
        "the slow-and-fast fixture must have both a fast and a slow plugin answer{run}"
    );

    let fastest = *first_result_by_plugin
        .values()
        .min()
        .expect("at least two plugins answered");
    let slowest = *first_result_by_plugin
        .values()
        .max()
        .expect("at least two plugins answered");
    assert!(
        fastest < slowest,
        "both plugins answered at the same moment, so the fixture cannot show independence{run}"
    );

    let shown_early = events(&records, "presentation")
        .into_iter()
        .filter(|event| event.at_ms(&run) < slowest)
        .filter(|event| event.number("visible_items", &run) > 0)
        .count();
    assert!(
        shown_early > 0,
        "nothing was presented in the {gap}ms between the fast plugin answering and the slow one \
         doing so: the slow plugin held the fast plugin's results{run}",
        gap = slowest - fastest,
    );

    let summary = summary(&records, &run);
    assert_eq!(
        summary_number(&summary, "fastest_plugin_first_result_ms", &run),
        fastest,
        "{run}"
    );
    assert_eq!(
        summary_number(&summary, "slowest_plugin_first_result_ms", &run),
        slowest,
        "{run}"
    );
    assert_eq!(
        summary_number(&summary, "presentations_before_slowest_first_result", &run),
        shown_early as u64,
        "{run}"
    );
}

// ---------------------------------------------------------------------------
// simulate-typing: the stress verdict (spec 31.4, 31.24, 31.25)
// ---------------------------------------------------------------------------

/// The rapid-typing stress workload the exit criteria are judged on.
fn stress_args<'a>(command: &'a str, query: &'a str, repeat: &'a str) -> Vec<&'a str> {
    vec![
        "dev",
        command,
        "--fixture",
        "rapid-typing",
        "--query",
        query,
        "--interval-ms",
        "3",
        "--repeat",
        repeat,
    ]
}

#[test]
fn rapid_typing_never_grows_the_queues_with_the_typing() {
    let query = typing_of_length(19);
    let (run, records) = traced(&stress_args("simulate-typing", &query, "20"));
    let summary = summary(&records, &run);

    let keystrokes = summary_number(&summary, "keystrokes", &run);
    assert_eq!(
        keystrokes,
        19 * 20,
        "the stress run did not type what it was asked to{run}"
    );

    for (capacity_field, depth_field) in [
        ("request_queue_capacity", "peak_queue_depth"),
        ("result_queue_capacity", "peak_result_queue_depth"),
    ] {
        let capacity = summary_number(&summary, capacity_field, &run);
        let depth = summary_number(&summary, depth_field, &run);

        assert!(capacity > 0, "`{capacity_field}=0` accepts nothing{run}");
        assert!(
            capacity < keystrokes,
            "`{capacity_field}={capacity}` is not a bound when {keystrokes} keystrokes fit inside \
             it{run}"
        );
        assert!(
            depth <= capacity,
            "`{depth_field}={depth}` exceeded `{capacity_field}={capacity}`{run}"
        );
        assert!(
            depth > 0,
            "`{depth_field}=0` after {keystrokes} keystrokes: the queue was never used, so its \
             bound is untested{run}"
        );
    }

    // Overflow was reached, and it was resolved by a named policy rather than
    // by growing.
    assert!(
        summary_number(&summary, "discarded_requests", &run) > 0,
        "{keystrokes} keystrokes fitted in the queues without a single request being coalesced, \
         dropped or refused{run}"
    );
    assert_eq!(
        summary_number(&summary, "discarded_requests", &run),
        summary_number(&summary, "coalesced_requests", &run)
            + summary_number(&summary, "dropped_obsolete_requests", &run)
            + summary_number(&summary, "rejected_plugin_queue_full", &run)
            + summary_number(&summary, "rejected_global_queue_full", &run),
        "`discarded_requests` is not the sum of the named ways a request is discarded, so some \
         requests vanished unaccounted for{run}"
    );
    // A request is one generation offered to one plugin, so the two outcomes
    // together can never exceed the offers that were made. An implementation
    // that queues a generation more than once per plugin fails here.
    let offered = summary_number(&summary, "generations", &run) * summary_number(&summary, "plugins", &run);
    let dispatched = summary_number(&summary, "dispatched_requests", &run);
    assert!(dispatched > 0, "the stress run dispatched nothing{run}");
    assert!(
        dispatched + summary_number(&summary, "discarded_requests", &run) <= offered,
        "{dispatched} dispatched plus {discarded} discarded exceeds the {offered} requests the \
         run could have made: something was queued twice{run}",
        discarded = summary_number(&summary, "discarded_requests", &run),
    );

    for (field, allowed) in [
        ("request_queue_overflow_policy", &REQUEST_QUEUE_POLICIES[..]),
        ("result_queue_overflow_policy", &RESULT_QUEUE_POLICIES[..]),
    ] {
        let reported = summary_field(&summary, field, &run);
        assert!(!reported.is_empty(), "`{field}` names no policy{run}");
        for policy in reported.split(',') {
            assert!(
                allowed.contains(&policy),
                "`{field}` reports `{policy}`, which is not one of {allowed:?}{run}"
            );
        }
    }

    assert_eq!(
        summary_number(&summary, "max_pending_per_plugin", &run),
        1,
        "{run}"
    );
}

#[test]
fn result_queue_and_backpressure_totals_are_observed_intake_events() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);
    let summary = summary(&records, &run);

    for (field, category) in [
        ("result_batches_admitted", "result_queue_admitted"),
        ("result_batches_merged", "result_queue_merged"),
        ("result_batches_rejected", "result_queue_rejected"),
        ("result_batches_evicted", "result_queue_evicted"),
        ("result_producer_pauses", "producer_paused"),
        ("result_producer_resumes", "producer_resumed"),
        ("result_merge_rejected", "result_merge_rejected"),
    ] {
        assert_eq!(
            summary_number(&summary, field, &run),
            events(&records, category).len() as u64,
            "`{field}` is not the count of observed `{category}` decisions{run}"
        );
    }

    let rejected = events(&records, "result_queue_rejected");
    assert!(!rejected.is_empty(), "the intake queue never overflowed{run}");
    let reasons: BTreeSet<&str> = rejected.iter().map(|event| event.need("reason", &run)).collect();
    assert!(
        reasons.contains("queue-full") && reasons.contains("low-priority-shed"),
        "the overflow probe observed {reasons:?}, not queue backpressure and priority shedding{run}"
    );
    assert!(
        summary_number(&summary, "result_producer_pauses", &run) > 0,
        "no producer was actually paused at its watermark{run}"
    );
    assert_eq!(
        summary_number(&summary, "result_producer_pauses", &run),
        summary_number(&summary, "result_producer_resumes", &run),
        "a paused fixture producer never resumed after the queue drained{run}"
    );
    assert!(
        summary_number(&summary, "result_batches_evicted", &run) > 0,
        "replace-oldest was named but never observed evicting a batch{run}"
    );
    assert_eq!(
        summary_number(&summary, "intake_events_dropped", &run),
        0,
        "the printed intake decisions are incomplete{run}"
    );
    for field in [
        "trace_events_dropped",
        "pipeline_errors_dropped",
        "trace_truncated",
    ] {
        assert_eq!(
            summary_number(&summary, field, &run),
            0,
            "`{field}` says the supposedly complete evidence was truncated{run}"
        );
    }
}

#[test]
fn rapid_typing_shows_nothing_stale_and_reorders_nothing() {
    let query = typing_of_length(19);
    let (run, records) = traced(&stress_args("simulate-typing", &query, "20"));
    let summary = summary(&records, &run);

    assert!(
        summary_number(&summary, "rejected_stale_results", &run) > 0,
        "the stress run produced no stale answers, so it cannot demonstrate they are rejected{run}"
    );
    assert!(
        summary_number(&summary, "cancelled_requests", &run) > 0,
        "the stress run cancelled nothing, so obsolete in-flight work was never invalidated{run}"
    );
    assert_eq!(
        summary_number(&summary, "stale_results_displayed", &run),
        0,
        "{run}"
    );
    assert_eq!(
        summary_number(&summary, "cross_generation_reorderings", &run),
        0,
        "{run}"
    );
}

#[test]
fn stale_display_totals_are_observed_from_published_frames() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);
    let summary = summary(&records, &run);
    let frames = events(&records, "frame");
    assert!(
        !frames.is_empty(),
        "the pipeline published no observable frames{run}"
    );
    assert!(
        frames.iter().any(|frame| frame.number("visible_items", &run) > 0),
        "only empty frames were observed, so stale-row exclusion was never exercised{run}"
    );

    let stale_items = frames
        .iter()
        .map(|frame| frame.number("stale_items", &run))
        .sum::<u64>();
    let visible_items = frames
        .iter()
        .map(|frame| frame.number("visible_items", &run))
        .sum::<u64>();
    assert_eq!(
        summary_number(&summary, "stale_results_displayed", &run),
        stale_items,
        "`stale_results_displayed` is not derived from the rows in observed frames{run}"
    );
    assert_eq!(
        summary_number(&summary, "presented_frames", &run),
        frames.len() as u64,
        "`presented_frames` is not the observed frame count{run}"
    );
    assert_eq!(
        summary_number(&summary, "presented_items", &run),
        visible_items,
        "`presented_items` is not the observed visible-row total{run}"
    );
    assert_eq!(stale_items, 0, "an obsolete generation reached a frame{run}");
}

#[test]
fn the_trace_and_the_stress_verdict_describe_the_same_run() {
    let query = typing_of_length(19);

    let (trace_run, trace_records) = traced(&stress_args("trace-query", &query, "20"));
    let (stress_run, stress_records) = traced(&stress_args("simulate-typing", &query, "20"));

    let traced_summary = summary(&trace_records, &trace_run);
    let stress_summary = summary(&stress_records, &stress_run);

    for field in REQUIRED_SUMMARY {
        assert_eq!(
            summary_field(&traced_summary, field, &trace_run),
            summary_field(&stress_summary, field, &stress_run),
            "`{field}` differs between the trace and the stress verdict for identical \
             arguments{trace_run}{stress_run}"
        );
    }

    assert!(
        all_events(&trace_records).len() > all_events(&stress_records).len(),
        "`trace-query` is the invocation that prints the trace{trace_run}{stress_run}"
    );
}

// ---------------------------------------------------------------------------
// Bad arguments are the caller's fault, not a crash
// ---------------------------------------------------------------------------

#[test]
fn an_unusable_argument_list_is_refused_with_usage_status() {
    let rejected: [&[&str]; 14] = [
        &["dev", "trace-query"],
        &["dev", "trace-query", "--fixture"],
        &["dev", "trace-query", "--fixture", "no-such-fixture"],
        &["dev", "trace-query", "--fixture", "rapid-typing", "--query"],
        &["dev", "trace-query", "--fixture", "rapid-typing", "--query", ""],
        &[
            "dev",
            "trace-query",
            "--fixture",
            "rapid-typing",
            "--interval-ms",
            "soon",
        ],
        &[
            "dev",
            "trace-query",
            "--fixture",
            "rapid-typing",
            "--interval-ms",
            "-5",
        ],
        &["dev", "trace-query", "--fixture", "rapid-typing", "--sideways"],
        &["dev", "simulate-typing"],
        &["dev", "simulate-typing", "--fixture", "no-such-fixture"],
        &["dev", "simulate-typing", "--fixture", "rapid-typing", "--repeat"],
        &[
            "dev",
            "simulate-typing",
            "--fixture",
            "rapid-typing",
            "--repeat",
            "0",
        ],
        &[
            "dev",
            "simulate-typing",
            "--fixture",
            "rapid-typing",
            "--repeat",
            "lots",
        ],
        &[
            "dev",
            "simulate-typing",
            "--fixture",
            "rapid-typing",
            "--interval-ms=",
        ],
    ];

    for args in rejected {
        let run = run(args);

        assert!(
            run.code.is_some(),
            "a bad argument list killed the process with a signal{run}"
        );
        assert!(
            !run.stderr.contains("panicked at"),
            "a bad argument list panicked instead of being refused{run}"
        );
        assert_ne!(
            run.code,
            Some(PANIC_STATUS),
            "a bad argument list unwound a panic{run}"
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
            "a refused run measured nothing, so it must report nothing on stdout{run}"
        );
    }
}

#[test]
fn oversized_workloads_are_rejected_before_trace_storage_can_rotate() {
    for (query, repeat) in [(typing_of_length(257), "1"), ("ab".to_owned(), "257")] {
        let run = run(&[
            "dev",
            "trace-query",
            "--fixture",
            "rapid-typing",
            "--query",
            &query,
            "--repeat",
            repeat,
        ]);
        assert_eq!(run.code, Some(EX_USAGE), "{run}");
        assert!(
            run.stderr.contains("limit"),
            "the refusal does not explain the deterministic workload bound{run}"
        );
        assert!(
            run.stdout.trim().is_empty(),
            "a refused workload cannot emit a partial trace{run}"
        );
    }
}

#[test]
fn a_refusal_names_the_fixtures_that_would_have_worked() {
    // The one case where a developer is closest to succeeding: they asked for
    // a fixture by name and got the name wrong. Sending them to `--help` when
    // the answer is a short list is a worse tool.
    for command in ["trace-query", "simulate-typing"] {
        let run = run(&["dev", command, "--fixture", "no-such-fixture"]);
        assert_eq!(run.code, Some(EX_USAGE), "{run}");

        for fixture in REQUIRED_FIXTURES {
            assert!(
                run.stderr.contains(fixture),
                "the refusal does not mention the `{fixture}` fixture{run}"
            );
        }
    }
}

#[test]
fn every_discarded_request_names_the_policy_that_discarded_it() {
    let (run, records) = traced(&["dev", "trace-query", "--fixture", "rapid-typing"]);
    let summary = summary(&records, &run);

    let dropped = events(&records, "request_dropped");
    assert!(
        !dropped.is_empty(),
        "rapid typing discarded no request, so the bounded queues were never exercised{run}"
    );
    for event in &dropped {
        let policy = event.need("policy", &run);
        assert!(
            REQUEST_QUEUE_POLICIES.contains(&policy),
            "line {line}: a request vanished under `policy={policy}`, which is not one of \
             {REQUEST_QUEUE_POLICIES:?}{run}",
            line = event.line,
        );
        event.need("plugin", &run);
    }

    assert_eq!(
        summary_number(&summary, "discarded_requests", &run),
        dropped.len() as u64,
        "the summary and the trace disagree about how many requests were discarded{run}"
    );
    assert_eq!(
        summary_number(&summary, "dispatched_requests", &run),
        events(&records, "dispatched").len() as u64,
        "the summary and the trace disagree about how many requests reached a plugin{run}"
    );
}
