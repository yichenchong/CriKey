//! `crikey` command-line entrypoint (spec 28).

use std::process::ExitCode;

use crikey_app::{App, SearchService, StartupStage};
use crikey_benchmarks::{
    run_catalog_benchmark, BenchmarkConfig, BenchmarkReport, PrefixLatency, STRESS_CATALOG_SIZE,
};
use crikey_core::PluginId;
use crikey_ui::{LauncherViewModel, NativeLauncher, NativeLauncherConfig, NativeLauncherEvent, UiEffect};

const USAGE: &str = "\
crikey - a fast, keyboard-driven application launcher

USAGE:
    crikey <COMMAND> [ARGS]

COMMANDS:
    run                             Start the launcher
    plugin list|install|remove|enable|disable|doctor|scheduling-profile
    dev   run|test|benchmark|trace-query|simulate-typing|inspect-protocol|test-legacy-compat
    package build|verify|inspect|migrate-keypirinha
    version                         Print version information
    help                            Print this message
";

/// `crikey dev` subcommands the usage advertises but nothing implements yet.
///
/// Kept apart from an unknown word on purpose: "advertised, not built" and "no
/// such subcommand" are different answers, and a script can tell them apart by
/// exit status without parsing prose.
const UNIMPLEMENTED_DEV: [&str; 6] = [
    "run",
    "test",
    "trace-query",
    "simulate-typing",
    "inspect-protocol",
    "test-legacy-compat",
];

/// Queries the reported percentiles are drawn from, and results each retains.
///
/// Fixed rather than exposed as options: two percentiles only compare when both
/// runs asked the same questions, and these are the numbers the stress-scale
/// test in `crikey-benchmarks` uses, so a report from this command and a report
/// from that test describe one workload rather than two.
const BENCHMARK_QUERIES: usize = 64;
const BENCHMARK_TOP_K: usize = 20;
const APPLICATION_CATALOG_PLUGIN: &str = "builtin.crikey.applications";
#[cfg(windows)]
const DEFAULT_ACTIVATION_HOTKEY: &str = "Ctrl+Alt+Space";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("help") | Some("-h") | Some("--help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("version") | Some("-V") | Some("--version") => {
            println!(
                "crikey {} ({} backend)",
                env!("CARGO_PKG_VERSION"),
                App::platform_backend_name()
            );
            ExitCode::SUCCESS
        }
        Some("dev") => dev(&args[1..]),
        Some("run") => run_launcher(&args[1..]),
        Some(command @ ("plugin" | "package")) => {
            eprintln!("crikey: `{command}` is not implemented yet");
            ExitCode::from(69) // EX_UNAVAILABLE
        }
        Some(other) => {
            eprintln!("crikey: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(64) // EX_USAGE
        }
    }
}

/// Starts the retained native launcher and wires immediate local search.
fn run_launcher(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("crikey: `run` takes no arguments\n\n{USAGE}");
        return ExitCode::from(64); // EX_USAGE
    }

    match run_native_launcher() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("crikey: launcher failed: {message}");
            ExitCode::from(70) // EX_SOFTWARE
        }
    }
}

fn run_native_launcher() -> Result<(), String> {
    let launcher = NativeLauncher::new(NativeLauncherConfig::default()).map_err(|error| error.to_string())?;
    let render_handle = launcher.handle();
    let mut search = SearchService::new(App::new());

    #[cfg(windows)]
    let has_activation_source = {
        let hotkey_handle = render_handle.clone();
        search
            .register_activation_hotkey(
                DEFAULT_ACTIVATION_HOTKEY,
                Box::new(move |_| {
                    let _ = hotkey_handle.request_toggle();
                }),
            )
            .map_err(|error| format!("cannot register {DEFAULT_ACTIVATION_HOTKEY}: {error}"))?;
        true
    };
    #[cfg(not(windows))]
    let has_activation_source = false;

    search
        .complete_stage(StartupStage::WindowAndHotkey)
        .map_err(|error| error.to_string())?;

    let owner = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
    let applications = search
        .discover_application_items(&owner)
        .map_err(|error| format!("application discovery failed: {error}"))?;
    search
        .replace_catalog(&owner, 1, applications)
        .map_err(|error| format!("application catalog was rejected: {error}"))?;
    search
        .complete_stage(StartupStage::PersistedCatalog)
        .map_err(|error| error.to_string())?;
    search
        .complete_stage(StartupStage::AcceptQueries)
        .map_err(|error| error.to_string())?;

    let activation_handle = render_handle.clone();
    activation_handle
        .request_activation()
        .map_err(|error| error.to_string())?;
    let mut view_model = LauncherViewModel::new();
    launcher
        .run(move |event| {
            let (command_session, effect) = match event {
                NativeLauncherEvent::Activated => {
                    // A rapid off/on hotkey pair may supersede the queued
                    // dismissal. Reset the old session before opening the new
                    // one so its query and rows cannot cross the boundary.
                    view_model.dismiss();
                    view_model.activate();
                    (None, None)
                }
                NativeLauncherEvent::Command { session, command } => {
                    (Some(session), view_model.apply(command))
                }
            };

            match effect {
                Some(UiEffect::Query(raw)) => {
                    if let Ok(generation) = search.submit_query(&raw) {
                        view_model.begin_generation(generation);
                        view_model.publish(generation, search.result_rows(), false);
                    }
                }
                Some(UiEffect::Dismissed) => {
                    if let Some(session) = command_session {
                        let _ = render_handle.request_hide_session(session);
                    }
                    // Without a registered reactivation source, retaining a
                    // hidden process would make the launcher unreachable.
                    if !has_activation_source {
                        let _ = render_handle.request_exit();
                    }
                }
                Some(UiEffect::Execute { item, action }) => match search.execute(&item, &action) {
                    Ok(()) => {
                        view_model.dismiss();
                        if let Some(session) = command_session {
                            let _ = render_handle.request_hide_session(session);
                        }
                        if !has_activation_source {
                            let _ = render_handle.request_exit();
                        }
                    }
                    Err(error) => {
                        let message = format!("Launch failed: {error}");
                        view_model.set_selected_status(message.clone());
                        eprintln!("crikey: {message}");
                    }
                },
                None => {}
            }

            if let Some(frame) = view_model.frame() {
                let _ = render_handle.submit_frame(&frame);
            }
        })
        .map_err(|error| error.to_string())
}

/// Routes a `crikey dev` invocation.
fn dev(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("benchmark") => benchmark(&args[1..]),
        Some(subcommand) if UNIMPLEMENTED_DEV.contains(&subcommand) => {
            eprintln!("crikey: `dev {subcommand}` is not implemented yet");
            ExitCode::from(69) // EX_UNAVAILABLE
        }
        Some(other) => {
            eprintln!("crikey: unknown dev subcommand `{other}`\n\n{USAGE}");
            ExitCode::from(64) // EX_USAGE
        }
        None => {
            eprintln!("crikey: `dev` needs a subcommand\n\n{USAGE}");
            ExitCode::from(64) // EX_USAGE
        }
    }
}

// ---------------------------------------------------------------------------
// dev benchmark
// ---------------------------------------------------------------------------

/// Usage for `crikey dev benchmark`.
///
/// Built rather than written out, so the documented default is the constant the
/// command actually uses. A usage line free to drift from the code is worse
/// than none: it is read as authoritative and cannot be checked.
fn benchmark_usage() -> String {
    format!(
        "\
crikey dev benchmark - measure the catalog path end to end (spec 25.1)

USAGE:
    crikey dev benchmark [--items N]

OPTIONS:
    --items N    Synthetic items to build, persist, reload and query
                 (default: {STRESS_CATALOG_SIZE}, the stress scale of spec 25.1)
    -h, --help   Print this message and measure nothing

Writes one `key=value` line per reported field to stdout, and exits non-zero if
the run did not measure the requested workload. The harness measures the archive
the launcher ships and nothing else: it produces no figure for any alternative
serialization format, because this workspace implements none to measure.
"
    )
}

/// What a parsed `crikey dev benchmark` argument list asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// Run the harness over this many synthetic items.
    Run(usize),
    /// Print the command's own usage and measure nothing.
    Usage,
}

/// Parses `crikey dev benchmark` arguments.
///
/// Only the item count is configurable, in both `--items N` and `--items=N`
/// spelling, and an unrecognized argument is refused rather than ignored: a
/// benchmark that silently discards half its invocation reports figures for a
/// workload nobody asked for. A repeated option takes its last value, as every
/// other tool does.
fn parse_benchmark_args(args: &[String]) -> Result<Request, String> {
    let mut items = STRESS_CATALOG_SIZE;
    let mut remaining = args.iter();

    while let Some(arg) = remaining.next() {
        let value = match arg.as_str() {
            "-h" | "--help" => return Ok(Request::Usage),
            "--items" => remaining.next().ok_or("`--items` needs a value")?.as_str(),
            other => other
                .strip_prefix("--items=")
                .ok_or_else(|| format!("unrecognized `dev benchmark` argument `{other}`"))?,
        };
        items = value
            .parse::<usize>()
            .map_err(|_| format!("`--items` needs a whole number of items, got `{value}`"))?;
    }

    if items == 0 {
        return Err("`--items` must be at least 1: an empty catalog measures nothing".to_owned());
    }
    Ok(Request::Run(items))
}

/// Runs the catalog benchmark harness and prints what it measured.
fn benchmark(args: &[String]) -> ExitCode {
    let items = match parse_benchmark_args(args) {
        Ok(Request::Usage) => {
            print!("{usage}", usage = benchmark_usage());
            return ExitCode::SUCCESS;
        }
        Ok(Request::Run(items)) => items,
        Err(message) => {
            eprintln!("crikey: {message}\n\n{usage}", usage = benchmark_usage());
            return ExitCode::from(64); // EX_USAGE
        }
    };

    let config = BenchmarkConfig {
        items,
        queries: BENCHMARK_QUERIES,
        top_k: BENCHMARK_TOP_K,
    };
    let report = run_catalog_benchmark(&config);

    // Printed before the verdict. A run that measured the wrong thing is still
    // evidence about what it measured, and stdout is where that record lives.
    print!("{lines}", lines = report_lines(&config, &report));

    match measurement_failure(&config, &report) {
        None => ExitCode::SUCCESS,
        Some(reason) => {
            eprintln!("crikey: the benchmark did not measure the requested workload: {reason}");
            ExitCode::from(70) // EX_SOFTWARE
        }
    }
}

/// The report as `key=value` lines, one per field.
///
/// The report is destructured rather than read field by field, so a field added
/// to [`BenchmarkReport`] stops this compiling instead of quietly going
/// unprinted. The workload is recorded alongside it because a percentile means
/// nothing without the query count it was drawn from, and a saved run has to
/// stay readable without the command line that produced it.
///
/// Every value is a bare decimal integer or the archive label, so no field ever
/// needs quoting and splitting on the first `=` is a complete reader.
fn report_lines(config: &BenchmarkConfig, report: &BenchmarkReport) -> String {
    let BenchmarkReport {
        format,
        items,
        unique_items,
        build_nanos,
        store_nanos,
        load_nanos,
        query_nanos_p50,
        query_nanos_p95,
        matched_total,
        candidates_examined,
        prefix_samples,
        prefix_latencies,
        archive_bytes,
        peak_rss_bytes_after_load,
        peak_rss_bytes_after_query,
        resident_bytes_after_load,
    } = report;

    let mut rendered = format!(
        "format={format}\n\
         config_items={config_items}\n\
         config_queries={config_queries}\n\
         config_top_k={config_top_k}\n\
         items={items}\n\
         unique_items={unique_items}\n\
         build_nanos={build_nanos}\n\
         store_nanos={store_nanos}\n\
         load_nanos={load_nanos}\n\
         query_nanos_p50={query_nanos_p50}\n\
         query_nanos_p95={query_nanos_p95}\n\
         matched_total={matched_total}\n\
         candidates_examined={candidates_examined}\n\
         prefix_samples={prefix_samples}\n\
         archive_bytes={archive_bytes}\n\
         peak_rss_bytes_after_load={peak_rss_bytes_after_load}\n\
         peak_rss_bytes_after_query={peak_rss_bytes_after_query}\n\
         resident_bytes_after_load={resident_bytes_after_load}\n",
        config_items = config.items,
        config_queries = config.queries,
        config_top_k = config.top_k,
    );
    for PrefixLatency {
        prefix_chars,
        samples,
        nanos_p50,
        nanos_p95,
        candidates_examined,
    } in prefix_latencies
    {
        rendered.push_str(&format!(
            "prefix_{prefix_chars}_samples={samples}\n\
             prefix_{prefix_chars}_nanos_p50={nanos_p50}\n\
             prefix_{prefix_chars}_nanos_p95={nanos_p95}\n\
             prefix_{prefix_chars}_candidates_examined={candidates_examined}\n"
        ));
    }
    rendered
}

/// Why the report does not describe the workload that was asked for, if it does
/// not.
///
/// The harness never fails loudly: a cache root it could not write answers with
/// an empty reload rather than a panic, which is right for a library and wrong
/// for a command whose entire output is then a column of figures describing
/// nothing. Only the round trip is checked, because that is the only part a
/// caller can state up front — nothing here is a latency or memory verdict.
fn measurement_failure(config: &BenchmarkConfig, report: &BenchmarkReport) -> Option<String> {
    if report.items != config.items {
        return Some(format!(
            "the persisted round trip returned {returned} of {asked} items",
            returned = report.items,
            asked = config.items,
        ));
    }
    if report.unique_items != config.items {
        return Some(format!(
            "the reloaded catalog holds {unique} distinct ids for {asked} items",
            unique = report.unique_items,
            asked = config.items,
        ));
    }
    if report.matched_total == 0 {
        return Some("the queries matched nothing, so the query phase timed an empty index".to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// A report of a run that went exactly as asked, for `config()`.
    fn config() -> BenchmarkConfig {
        BenchmarkConfig {
            items: 4,
            queries: 2,
            top_k: 1,
        }
    }

    fn sound_report() -> BenchmarkReport {
        BenchmarkReport {
            format: "test-archive",
            items: 4,
            unique_items: 4,
            build_nanos: 11,
            store_nanos: 22,
            load_nanos: 33,
            query_nanos_p50: 44,
            query_nanos_p95: 55,
            matched_total: 2,
            candidates_examined: 3,
            prefix_samples: 2,
            prefix_latencies: vec![PrefixLatency {
                prefix_chars: 1,
                samples: 2,
                nanos_p50: 41,
                nanos_p95: 51,
                candidates_examined: 3,
            }],
            archive_bytes: 66,
            peak_rss_bytes_after_load: 77,
            peak_rss_bytes_after_query: 88,
            resident_bytes_after_load: 99,
        }
    }

    #[test]
    fn the_benchmark_defaults_to_the_stress_scale_catalog() {
        assert_eq!(
            parse_benchmark_args(&args(&[])),
            Ok(Request::Run(STRESS_CATALOG_SIZE))
        );
    }

    #[test]
    fn the_item_count_is_accepted_in_either_spelling_and_the_last_one_wins() {
        assert_eq!(
            parse_benchmark_args(&args(&["--items", "2048"])),
            Ok(Request::Run(2_048))
        );
        assert_eq!(
            parse_benchmark_args(&args(&["--items=2048"])),
            Ok(Request::Run(2_048))
        );
        assert_eq!(
            parse_benchmark_args(&args(&["--items=1", "--items", "9"])),
            Ok(Request::Run(9))
        );
    }

    #[test]
    fn asking_for_help_measures_nothing() {
        assert_eq!(parse_benchmark_args(&args(&["--help"])), Ok(Request::Usage));
        assert_eq!(parse_benchmark_args(&args(&["-h"])), Ok(Request::Usage));
        // Help wins over an item count that would otherwise be refused: the
        // reply to "how do I use this" is never an error about using it wrong.
        assert_eq!(
            parse_benchmark_args(&args(&["--items=0", "-h"])),
            Ok(Request::Usage)
        );
    }

    #[test]
    fn the_benchmark_refuses_a_workload_it_cannot_run() {
        for rejected in [
            vec!["--items"],
            vec!["--items", "-1"],
            vec!["--items", "two"],
            vec!["--items", "--help"],
            vec!["--items", "0"],
            vec!["--items=0"],
            vec!["--queries", "8"],
            vec!["500000"],
            vec![""],
        ] {
            assert!(
                parse_benchmark_args(&args(&rejected)).is_err(),
                "`{rejected:?}` is not a workload this command can honour"
            );
        }
    }

    #[test]
    fn the_usage_documents_the_default_the_parser_actually_applies() {
        let usage = benchmark_usage();
        assert!(
            usage.contains(&STRESS_CATALOG_SIZE.to_string()),
            "the documented default must be the one `--items` falls back to: {usage}"
        );
    }

    #[test]
    fn every_reported_field_is_printed_as_one_key_equals_value_line() {
        let rendered = report_lines(&config(), &sound_report());

        assert_eq!(
            rendered,
            "format=test-archive\n\
             config_items=4\n\
             config_queries=2\n\
             config_top_k=1\n\
             items=4\n\
             unique_items=4\n\
             build_nanos=11\n\
             store_nanos=22\n\
             load_nanos=33\n\
             query_nanos_p50=44\n\
             query_nanos_p95=55\n\
             matched_total=2\n\
             candidates_examined=3\n\
             prefix_samples=2\n\
             archive_bytes=66\n\
             peak_rss_bytes_after_load=77\n\
             peak_rss_bytes_after_query=88\n\
             resident_bytes_after_load=99\n\
             prefix_1_samples=2\n\
             prefix_1_nanos_p50=41\n\
             prefix_1_nanos_p95=51\n\
             prefix_1_candidates_examined=3\n"
        );
    }

    #[test]
    fn the_printed_report_is_splittable_without_quoting_rules() {
        let rendered = report_lines(&config(), &sound_report());

        let mut keys = Vec::new();
        for line in rendered.lines() {
            let (key, value) = line.split_once('=').expect("every line is one key=value pair");
            assert!(!key.is_empty(), "`{line}` carries no key");
            assert!(!value.is_empty(), "`{line}` carries no value");
            assert!(!value.contains('='), "`{line}` splits ambiguously");
            keys.push(key);
        }

        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(
            unique.len(),
            keys.len(),
            "a repeated key would shadow a field: {keys:?}"
        );
    }

    #[test]
    fn a_sound_round_trip_is_not_a_failure() {
        assert_eq!(measurement_failure(&config(), &sound_report()), None);
    }

    #[test]
    fn a_report_that_describes_another_workload_is_a_failure() {
        // The harness answers an unwritable cache with an empty reload rather
        // than a panic. The figures are real; they are not a measurement of the
        // catalog that was asked for, and the exit status has to say so.
        let empty = BenchmarkReport {
            items: 0,
            unique_items: 0,
            matched_total: 0,
            ..sound_report()
        };
        assert!(measurement_failure(&config(), &empty).is_some());

        let lossy = BenchmarkReport {
            unique_items: 3,
            ..sound_report()
        };
        assert!(measurement_failure(&config(), &lossy).is_some());

        let unsearchable = BenchmarkReport {
            matched_total: 0,
            ..sound_report()
        };
        assert!(measurement_failure(&config(), &unsearchable).is_some());
    }

    #[test]
    fn dev_separates_an_unbuilt_subcommand_from_an_unknown_one() {
        // Every subcommand the top-level usage advertises is either implemented
        // or listed as unbuilt; a word in neither set is a typo, not a promise.
        for advertised in UNIMPLEMENTED_DEV {
            assert!(
                USAGE.contains(advertised),
                "`dev {advertised}` is reported as unbuilt but never advertised"
            );
        }
        assert!(
            !UNIMPLEMENTED_DEV.contains(&"benchmark"),
            "`dev benchmark` is implemented and must not be reported as unbuilt"
        );
    }
}
