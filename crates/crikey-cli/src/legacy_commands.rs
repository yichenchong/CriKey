//! The three M3 legacy developer commands (spec 26.3, 28).
//!
//! * `crikey dev test-legacy-compat --package PATH` runs the legacy scheduling
//!   conformance suite of spec 14.5, 14.8, 14.9 and 14.12 against one package
//!   and reports a verdict plus one named result per rule.
//! * `crikey dev inspect-catalog --package PATH` reports the catalog a legacy
//!   package publishes, without opening a window.
//! * `crikey dev compatibility-report` names the two version-controlled data
//!   files of spec 14.10 and 27.4 and prints the counts folded out of them.
//!
//! # The output contract
//!
//! Every line is whitespace-separated `key=value` tokens, exactly the shape
//! `crikey dev benchmark`, `trace-query` and `simulate-typing` already emit, so
//! `cut`, `grep` and `sort` are a complete reader. Legacy item labels are
//! written by plugin authors, so a value may hold a space, an `=` or a `%`;
//! every value is therefore percent-encoded with uppercase hex ([`encode`]).
//! Replacing spaces with underscores would also parse, and would quietly
//! corrupt the one thing catalog inspection exists to show.
//!
//! # Three exit statuses, not two
//!
//! A conformance *failure* is a result rather than a refusal: the command ran,
//! learned something, and prints all of it before exiting
//! [`EX_NOT_CONFORMANT`]. A bad argument list or an unloadable package is the
//! caller's fault and exits [`EX_USAGE`] with an empty stdout. `EX_UNAVAILABLE`
//! belongs to subcommands that are advertised and unbuilt; all three of these
//! are built, so none may ever answer it. A CI job that cannot tell "this
//! plugin is incompatible" from "you typed the flag wrong" reports both as red.
//!
//! # Determinism
//!
//! Nothing here sleeps or samples a clock, and no printed value carries a
//! process id, a duration or a temporary path: two runs of one invocation must
//! be byte-identical, the failing ones included, or a saved report cannot be
//! diffed against the last release — which is the only use a compatibility
//! corpus has. Scheduling time is virtual throughout; the only wall-clock
//! bounds are the worker's startup and per-call budgets, which are values
//! passed in rather than clocks read here.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use crikey_core::{ArgumentPolicy, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_input_scheduler::{Millis, SchedulingProfile};
use crikey_legacy_compat::{
    discover_interpreter, CallbackObservation, CompatibilityMatrix, CompatibilityReport, Delivery,
    DiagnosticLimits, ImportOutcome, InstanceId, Interpreter, LegacyCallback, LegacyDeadlines,
    LegacyDiagnostics, LegacyOutcome, LegacyPackage, LegacyRequest, LegacyRequestKind, LegacyResponse,
    LegacyRuntime, LegacyTraceEvent, LegacyWorker, LegacyWorkerHandle, PackageLoader, PluginClassification,
    PluginCorpus, TerminationReason, WorkerError, WorkerOptions, MINIMUM_SUPPORTED_PYTHON, WORKER_ENTRY_FILE,
};
use crikey_python_host::RuntimeProfile;

/// A completed run that found nothing wrong.
const EX_OK: u8 = 0;
/// A conformance run that completed and reached a non-passing verdict. The
/// whole report is still on stdout; only the status differs.
const EX_NOT_CONFORMANT: u8 = 1;
/// `EX_USAGE`: the caller's fault, and the only status a bad argument list or an
/// unloadable package may produce.
const EX_USAGE: u8 = 64;
/// `EX_SOFTWARE`: this host could not run the command at all — no supported
/// CPython, no shim on disk, an unreadable data file. Never the caller's fault
/// and never a conformance verdict.
const EX_SOFTWARE: u8 = 70;

/// Workspace-relative source of the API classifications (spec 14.10).
const MATRIX_PATH: &str = "compatibility/api-matrix/matrix.toml";
/// Workspace-relative source of the corpus classifications (spec 27.4).
const CORPUS_PATH: &str = "compatibility/real-plugin-corpus/corpus.toml";

/// Bound on the startup handshake with the child interpreter, in milliseconds.
/// A liveness guard rather than a performance assertion: it turns a shim that
/// never answers into a named error instead of a hung developer command.
const STARTUP_BUDGET_MS: u64 = 30_000;
/// Bound on one legacy callback. Generous because spec 9.6 explicitly permits a
/// slow legacy callback and forbids killing the worker for being slow; the
/// `ignores-should-terminate` fixture answers late on purpose, and nothing in
/// the report may depend on a timeout firing.
const CALL_BUDGET_MS: u64 = 120_000;

/// The one Windows-only module of the documented compatibility surface
/// (spec 14.2, 14.12). Detected from the package's own `import`, never from a
/// self-report: a package could omit or misstate a declaration, and the report
/// would then be wrong in the direction that matters — presenting a package
/// that needs Win32 as cross-platform (acceptance 31.31).
const WINDOWS_ONLY_MODULE: &str = "keypirinha_wintypes";

/// Most package modules the static import scan reads. A legacy package is
/// third-party content of unknown provenance, so the scan is bounded by
/// construction; modules past the cap are not scanned.
const MAX_SCANNED_MODULES: usize = 512;
/// Largest module the scan reads, in bytes. A larger file is skipped rather
/// than buffered: an import statement lives in the first few lines, and a
/// developer command must not be a way to make CriKey hold an arbitrary file.
const MAX_SCANNED_MODULE_BYTES: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// The conformance suite
// ---------------------------------------------------------------------------

/// Checks every legacy package is put through, in report order.
///
/// One named check per rule, because "this package failed legacy conformance"
/// is not a bug report: the maintainer's next question is always *which* rule,
/// and a blanket verdict sends them back to the spec to guess.
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
/// nothing to say about Win32 entry points for a package that never names one,
/// and an `unavailable` line there would make every package on a non-Windows
/// host `incomplete`.
const WIN32_CHECK: &str = "win32_entry_points_operational";

// ---------------------------------------------------------------------------
// The virtual timeline
//
// Scheduling time is an explicit millisecond value throughout. The three
// consecutive query timestamps are one millisecond apart, well inside any
// plausible debounce window (spec 25.4 bands start at tens of milliseconds), so
// a host that debounced legacy keystrokes would be caught here.
// ---------------------------------------------------------------------------

const T_BOOT: Millis = 0;
const T_CATALOG_FIRST: Millis = 10;
const T_CATALOG_SECOND: Millis = 20;
const T_QUERY_EMPTY: Millis = 30;
const T_QUERY_ALPHA: Millis = 31;
const T_QUERY_BETA: Millis = 32;
const T_SELECT: Millis = 40;
const T_ARGUMENT_ANSWER: Millis = 41;
const T_SERIAL_DISPATCHED: Millis = 100;
const T_SERIAL_SECOND: Millis = 110;
const T_SERIAL_THIRD: Millis = 120;
const T_STALE_ANSWER: Millis = 130;
const T_RESUME: Millis = 140;
const T_RESUME_ANSWER: Millis = 141;
const T_ORPHAN_BUILD: Millis = 150;
const T_RELOAD: Millis = 160;
const T_ORPHAN_ANSWER: Millis = 170;
const T_SHUTDOWN: Millis = 180;

/// Queries the suite types. Distinct and query-shaped, so a plugin that merely
/// echoes its input cannot accidentally produce two identical payloads and be
/// reported as caching (spec 14.9).
const QUERY_ALPHA: &str = "crikey-conformance-alpha";
const QUERY_BETA: &str = "crikey-conformance-beta";
const QUERY_SERIAL_FIRST: &str = "crikey-conformance-serial-one";
const QUERY_SERIAL_SECOND: &str = "crikey-conformance-serial-two";
const QUERY_SERIAL_THIRD: &str = "crikey-conformance-serial-three";

/// Deadline ladder for the run (spec 9.6). The modern hard query budget sits
/// deliberately far below the legacy ladder, so a legacy callback that outran a
/// modern plugin's budget is visibly *not* killed for it.
fn deadlines() -> LegacyDeadlines {
    LegacyDeadlines {
        modern_hard_query_ms: 250,
        soft_warning_ms: 5_000,
        hung_worker_ms: 120_000,
        teardown_ms: 250,
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub(crate) fn test_legacy_compat(args: &[String]) -> ExitCode {
    let command = "test-legacy-compat";
    let package_path = match parse_package_args(command, args) {
        Ok(Some(path)) => path,
        Ok(None) => {
            print!("{}", package_help(command, CONFORMANCE_SYNOPSIS));
            return ExitCode::from(EX_OK);
        }
        Err(message) => return refuse(command, &message),
    };

    let (package, interpreter) = match open_package(command, &package_path) {
        Ok(opened) => opened,
        Err(refusal) => return refusal,
    };

    let dependency = match scan_windows_only_dependency(&package) {
        Ok(found) => found,
        Err(message) => {
            eprintln!("crikey: dev {command}: cannot read `{package_path}`: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };

    // The corpus is the project's own published verdict on this package (spec
    // 27.4), and it has to reach the `portable` field below. The two
    // Windows-only packages it references get to Win32 through bundled COM,
    // `ctypes` and `sc.exe` rather than through `keypirinha_wintypes`, so the
    // import scan above sees nothing and a report built on the scan alone would
    // advertise them as cross-platform — the exact misrepresentation acceptance
    // 31.31 forbids. An unreadable corpus is neither a conformance verdict nor
    // the caller's fault, so it exits `EX_SOFTWARE` rather than being guessed at.
    let corpus = match PluginCorpus::load(&workspace_root().join(CORPUS_PATH)) {
        Ok(corpus) => corpus,
        Err(error) => {
            eprintln!("crikey: dev {command}: cannot read `{CORPUS_PATH}`: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let declared = declared_classification(&corpus, &package);

    let ConformanceRun {
        mut checks,
        obsolete_callback,
        undocumented,
    } = run_conformance(&package, &interpreter);
    classify_portability(&mut checks, dependency.as_deref());

    // The verdict is folded out of the very results the report prints, so the
    // summary and the detail can never describe two different runs.
    let mut passed = 0_u64;
    let mut failed = 0_u64;
    let mut unavailable = 0_u64;
    let mut detail_lines = String::new();
    for name in CORE_CHECKS.iter().copied().chain([WIN32_CHECK]) {
        let Some(check) = checks.get(name) else {
            continue;
        };
        match check.result {
            CheckResult::Pass => passed += 1,
            CheckResult::Fail => failed += 1,
            CheckResult::Unavailable => unavailable += 1,
        }
        writeln!(
            detail_lines,
            "check={name} result={result} detail={detail}",
            result = check.result.as_str(),
            detail = encode(&check.detail),
        )
        .expect("writing to a String cannot fail");
    }

    let verdict = if failed > 0 {
        "fail"
    } else if unavailable > 0 {
        // Nothing broke, but we did not look everywhere. Calling that a pass
        // would report Win32 coverage this host never had, and a green tick
        // meaning "we did not look" is the plausible lie the roadmap forbids.
        "incomplete"
    } else {
        "pass"
    };

    let mut report = String::new();
    field(&mut report, "command", command);
    field(&mut report, "package", &package_path);
    field(&mut report, "package_id", package.id.as_str());
    field(&mut report, "platform", host_platform());
    field(&mut report, "interpreter", &interpreter.path().to_string_lossy());
    field(&mut report, "python_version", &interpreter.version().to_string());
    field(&mut report, "scheduling_profile", "legacy-strict");
    // The §26.2 diagnostics store is fed nowhere else on a live path, so spec
    // 26.2 ("CriKey should report…") and acceptance 31.29 are unmet in practice
    // until it is fed here. Everything folded in is a fact this command already
    // established: the profile it ran under, the package's own imports, the
    // interpreter it resolved, the corpus's published classification, and —
    // from the conformance run — a superseded callback that never polled
    // `should_terminate()`.
    //
    // `portable` is then read back out of the store rather than recomputed, so
    // the one-word claim and the findings printed beside it can never disagree.
    let plugin = plugin_of(&package);
    let diagnostics = compatibility_diagnostics(
        &plugin,
        &package,
        &interpreter,
        dependency.as_deref(),
        declared,
        obsolete_callback,
        &undocumented,
    );
    field(
        &mut report,
        "portable",
        if diagnostics.is_portable(&plugin) {
            "true"
        } else {
            "false"
        },
    );
    field(
        &mut report,
        "checks_total",
        &(passed + failed + unavailable).to_string(),
    );
    field(&mut report, "checks_passed", &passed.to_string());
    field(&mut report, "checks_failed", &failed.to_string());
    field(&mut report, "checks_unavailable", &unavailable.to_string());
    field(&mut report, "verdict", verdict);

    render_diagnostics(&mut report, &diagnostics, &plugin);
    report.push_str(&detail_lines);

    print!("{report}");
    ExitCode::from(if verdict == "pass" {
        EX_OK
    } else {
        EX_NOT_CONFORMANT
    })
}

pub(crate) fn inspect_catalog(args: &[String]) -> ExitCode {
    let command = "inspect-catalog";
    let package_path = match parse_package_args(command, args) {
        Ok(Some(path)) => path,
        Ok(None) => {
            print!("{}", package_help(command, CATALOG_SYNOPSIS));
            return ExitCode::from(EX_OK);
        }
        Err(message) => return refuse(command, &message),
    };

    let (package, interpreter) = match open_package(command, &package_path) {
        Ok(opened) => opened,
        Err(refusal) => return refusal,
    };

    let items = match publish_catalog(&package, &interpreter) {
        Ok(items) => items,
        Err(message) => {
            eprintln!("crikey: dev {command}: `{package_path}`: {message}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };

    let mut report = String::new();
    field(&mut report, "command", command);
    field(&mut report, "package", &package_path);
    field(&mut report, "package_id", package.id.as_str());
    field(&mut report, "interpreter", &interpreter.path().to_string_lossy());
    field(&mut report, "python_version", &interpreter.version().to_string());
    field(&mut report, "scheduling_profile", "legacy-strict");
    field(&mut report, "items", &items.len().to_string());

    // Catalog inspection is the other place a legacy plugin's compatibility
    // problems become visible to a developer, so it feeds the §26.2 store too.
    // Both inputs are best-effort here: neither an unreadable module nor an
    // unreadable corpus may turn `inspect-catalog` into a refusal, because this
    // command prints no portability verdict that could be wrong without them.
    let plugin = plugin_of(&package);
    let dependency = scan_windows_only_dependency(&package).ok().flatten();
    let declared = PluginCorpus::load(&workspace_root().join(CORPUS_PATH))
        .ok()
        .and_then(|corpus| declared_classification(&corpus, &package));
    let diagnostics = compatibility_diagnostics(
        &plugin,
        &package,
        &interpreter,
        dependency.as_deref(),
        declared,
        None,
        &[],
    );
    render_diagnostics(&mut report, &diagnostics, &plugin);
    for (index, item) in items.iter().enumerate() {
        report.push_str(&item_line(index, item));
    }

    print!("{report}");
    ExitCode::from(EX_OK)
}

pub(crate) fn compatibility_report(args: &[String]) -> ExitCode {
    let command = "compatibility-report";
    match parse_no_arguments(command, args) {
        Ok(true) => {}
        Ok(false) => {
            print!("{}", report_help(command));
            return ExitCode::from(EX_OK);
        }
        Err(message) => return refuse(command, &message),
    }

    let root = workspace_root();
    let matrix = match CompatibilityMatrix::load(&root.join(MATRIX_PATH)) {
        Ok(matrix) => matrix,
        Err(error) => {
            eprintln!("crikey: dev {command}: cannot read `{MATRIX_PATH}`: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    let corpus = match PluginCorpus::load(&root.join(CORPUS_PATH)) {
        Ok(corpus) => corpus,
        Err(error) => {
            eprintln!("crikey: dev {command}: cannot read `{CORPUS_PATH}`: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };

    // The paths come first, workspace-relative and with forward slashes, so two
    // people on two machines can check they read the same file. There is no
    // `command=` line: this report is a fold over version-controlled data and
    // has no invocation-specific identity to echo.
    let mut out = String::new();
    field(&mut out, "matrix_path", MATRIX_PATH);
    field(&mut out, "corpus_path", CORPUS_PATH);
    out.push_str(&CompatibilityReport::new(&matrix, &corpus).render());

    print!("{out}");
    ExitCode::from(EX_OK)
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

const CONFORMANCE_SYNOPSIS: &str = "\
Runs the legacy scheduling conformance suite (spec 14.5, 14.8, 14.9, 14.12)
against one legacy package and reports a verdict plus one named result per rule.
A conformance failure exits 1 with the whole report on stdout; a bad argument
list or an unloadable package exits 64 and reports nothing.";

const CATALOG_SYNOPSIS: &str = "\
Reports the catalog a legacy package publishes: one line per item carrying every
field of spec 10.1. The plugin runs in a child interpreter and no window is
opened, so this works on a host with no display.";

/// Parses a `--package PATH` argument list.
///
/// `Ok(None)` means help was asked for. Help is honoured *before* anything is
/// validated: `--help` alongside a package that could not be loaded must still
/// explain the command rather than refuse the package, because a developer
/// asking how to invoke a command has not yet claimed the path is good.
fn parse_package_args(command: &str, args: &[String]) -> Result<Option<String>, String> {
    if wants_help(args) {
        return Ok(None);
    }

    let mut package: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        if let Some(value) = argument.strip_prefix("--package=") {
            package = Some(value.to_owned());
            index += 1;
        } else if argument == "--package" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("`dev {command}` needs a path after `--package`"))?;
            package = Some(value.clone());
            index += 2;
        } else {
            // Refused rather than ignored: a conformance run that silently
            // discarded half its invocation reports on a package nobody named.
            return Err(format!(
                "`dev {command}` does not understand `{argument}`; the package is named with \
                 `--package PATH`"
            ));
        }
    }

    match package {
        // An empty path is refused rather than resolved: it would otherwise
        // become the process working directory, and the command would report on
        // whatever the developer happened to be standing in.
        Some(path) if path.is_empty() => Err(format!("`dev {command} --package` was given an empty path")),
        Some(path) => Ok(Some(path)),
        None => Err(format!("`dev {command}` needs `--package PATH`")),
    }
}

/// `Ok(true)` to run, `Ok(false)` for help.
fn parse_no_arguments(command: &str, args: &[String]) -> Result<bool, String> {
    if wants_help(args) {
        return Ok(false);
    }
    match args.first() {
        None => Ok(true),
        Some(unexpected) => Err(format!(
            "`dev {command}` takes no arguments: it reads the version-controlled \
             `{MATRIX_PATH}` and `{CORPUS_PATH}`, so there is nothing to point it at, and \
             `{unexpected}` was given"
        )),
    }
}

fn wants_help(args: &[String]) -> bool {
    args.iter()
        .any(|argument| argument == "-h" || argument == "--help")
}

fn refuse(command: &str, message: &str) -> ExitCode {
    eprintln!("crikey: {message}\n\ncrikey dev {command} --help");
    ExitCode::from(EX_USAGE)
}

fn package_help(command: &str, synopsis: &str) -> String {
    format!(
        "crikey dev {command}\n\
         \n\
         USAGE:\n\
         \x20   crikey dev {command} --package PATH\n\
         \x20   crikey dev {command} --help\n\
         \n\
         OPTIONS:\n\
         \x20   --package PATH   A legacy package directory or `.keypirinha-package` archive.\n\
         \x20   -h, --help       Print this message and inspect nothing.\n\
         \n\
         {synopsis}\n\
         \n\
         Output is whitespace-separated `key=value` tokens with percent-encoded values.\n"
    )
}

fn report_help(command: &str) -> String {
    format!(
        "crikey dev {command}\n\
         \n\
         USAGE:\n\
         \x20   crikey dev {command}\n\
         \x20   crikey dev {command} --help\n\
         \n\
         OPTIONS:\n\
         \x20   -h, --help       Print this message and read nothing.\n\
         \n\
         Names the two version-controlled compatibility data files it read and prints the\n\
         classification counts folded out of them (spec 14.10, 27.4):\n\
         \x20   {MATRIX_PATH}\n\
         \x20   {CORPUS_PATH}\n\
         \n\
         Output is whitespace-separated `key=value` tokens with percent-encoded values.\n"
    )
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Loads the package and resolves the interpreter, or refuses by name.
///
/// Every refusal repeats the path exactly as the caller wrote it: a refusal that
/// cannot be traced back to its input is one a script cannot act on (spec 26.2),
/// and a missing path must never be a panic.
fn open_package(command: &str, path: &str) -> Result<(LegacyPackage, Interpreter), ExitCode> {
    let loader = PackageLoader::new(std::env::temp_dir().join("crikey-dev-legacy-packages"));
    let package = match loader.load(Path::new(path)) {
        Ok(package) => package,
        Err(error) => {
            eprintln!("crikey: dev {command}: cannot load legacy package `{path}`: {error}");
            return Err(ExitCode::from(EX_USAGE));
        }
    };

    // Discovery failing is the host's problem rather than the caller's, so it is
    // not an `EX_USAGE` refusal — and never a skip: a host that cannot run
    // CPython cannot run the Legacy Compatibility Layer, and saying so is the
    // only honest answer (spec 14.11).
    let interpreter = match discover_interpreter(&RuntimeProfile::LegacyCompatibility) {
        Ok(interpreter) => interpreter,
        Err(error) => {
            eprintln!("crikey: dev {command}: no supported CPython for the legacy worker: {error}");
            return Err(ExitCode::from(EX_SOFTWARE));
        }
    };

    Ok((package, interpreter))
}

fn plugin_of(package: &LegacyPackage) -> PluginId {
    PluginId(format!("legacy.{}", package.id))
}

fn worker_options(plugin: &PluginId) -> Result<WorkerOptions, String> {
    let shim = crikey_legacy_compat::worker::shim_root();
    if !shim.join(WORKER_ENTRY_FILE).is_file() {
        return Err(format!(
            "the legacy worker entry `{WORKER_ENTRY_FILE}` is not in `{}`; point \
             CRIKEY_LEGACY_SHIM_DIR at the shim package directory",
            shim.display()
        ));
    }
    Ok(WorkerOptions::new(plugin.clone(), shim)
        .with_startup_timeout_ms(STARTUP_BUDGET_MS)
        .with_call_timeout_ms(CALL_BUDGET_MS))
}

/// A request built outside the runtime, for the direct worker calls catalog
/// inspection makes. Catalog work carries no query generation: it is not query
/// work and is never subject to query staleness (spec 14.8).
fn direct_request(plugin: &PluginId, kind: LegacyRequestKind) -> LegacyRequest {
    LegacyRequest {
        plugin: plugin.clone(),
        instance: InstanceId(1),
        generation: Generation::ZERO,
        kind,
    }
}

fn published_items(outcome: &LegacyOutcome) -> Option<&[Item]> {
    match outcome {
        LegacyOutcome::SetCatalog(items)
        | LegacyOutcome::MergeCatalog(items)
        | LegacyOutcome::Suggestions(items) => Some(items),
        _ => None,
    }
}

/// Starts the plugin in a child interpreter and returns the catalog it
/// publishes.
fn publish_catalog(package: &LegacyPackage, interpreter: &Interpreter) -> Result<Vec<Item>, String> {
    let plugin = plugin_of(package);
    let mut worker = LegacyWorker::spawn(interpreter, package, worker_options(&plugin)?)
        .map_err(|error| format!("the legacy worker did not start: {error}"))?;

    let collected = catalog_of(&mut worker, &plugin);

    if let Err(error) = worker.shutdown() {
        // Reported, never fatal: the catalog is already in hand, and a package
        // whose teardown is untidy still has a catalog worth showing.
        eprintln!("crikey: dev inspect-catalog: worker shutdown: {error}");
    }
    collected
}

/// Runs the two callbacks a catalog needs, in the only order that is correct.
///
/// `on_start` comes first because one-time initialization is where a legacy
/// plugin reads its settings, and a catalog built before it ran would be the
/// catalog of an unconfigured plugin (spec 14.8).
fn catalog_of(worker: &mut LegacyWorker, plugin: &PluginId) -> Result<Vec<Item>, String> {
    worker
        .call(direct_request(plugin, LegacyRequestKind::Start))
        .map_err(|error| format!("on_start failed: {error}"))?;
    let response = worker
        .call(direct_request(plugin, LegacyRequestKind::Catalog))
        .map_err(|error| format!("on_catalog failed: {error}"))?;
    match published_items(&response.outcome) {
        Some(items) => Ok(items.to_vec()),
        None => Err(format!("on_catalog published no catalog: {:?}", response.outcome)),
    }
}

// ---------------------------------------------------------------------------
// Static portability scan (spec 14.12, acceptance 31.31)
// ---------------------------------------------------------------------------

/// The published corpus's classification of `package`, if the corpus references
/// it (spec 27.4).
///
/// Matched on the package id, which is the corpus's own key. A package nobody
/// has referenced yields `None` rather than `untested`: an absent entry is the
/// absence of a classification, and reading it as one would file a finding
/// about data that does not exist.
fn declared_classification(corpus: &PluginCorpus, package: &LegacyPackage) -> Option<PluginClassification> {
    corpus
        .entries()
        .iter()
        .find(|entry| entry.id == package.id.as_str())
        .map(|entry| entry.classification)
}

/// The Windows-only dependency this package declares, if any, named together
/// with the module it was found in.
fn scan_windows_only_dependency(package: &LegacyPackage) -> Result<Option<String>, String> {
    let content_root = package.root.content_root();
    for module in package.modules.iter().take(MAX_SCANNED_MODULES) {
        let path = content_root.join(&module.relative_path);
        let size = fs::metadata(&path)
            .map_err(|error| format!("cannot read module `{}`: {error}", module.import_name))?
            .len();
        if size > MAX_SCANNED_MODULE_BYTES {
            continue;
        }
        let Ok(source) = fs::read_to_string(&path) else {
            // Not valid UTF-8, so not a module CPython would import as source
            // either. Skipped rather than refused: it cannot carry an import.
            continue;
        };
        if imports_windows_only(&source) {
            return Ok(Some(format!(
                "{WINDOWS_ONLY_MODULE} imported by {}",
                module.import_name
            )));
        }
    }
    Ok(None)
}

/// The first Windows-only Win32 entry point the package reaches, if any — the
/// attribute a developer has to guard (spec 14.12). Best-effort and bounded
/// exactly like [`scan_windows_only_dependency`]; the documented portability
/// probe `is_available` is never counted, because it is the guard a portable
/// plugin is supposed to use rather than an entry point that needs guarding.
fn scan_windows_only_entry_point(package: &LegacyPackage) -> Option<String> {
    let content_root = package.root.content_root();
    for module in package.modules.iter().take(MAX_SCANNED_MODULES) {
        let path = content_root.join(&module.relative_path);
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.len() > MAX_SCANNED_MODULE_BYTES {
            continue;
        }
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(entry_point) = windows_only_entry_point_in(&source) {
            return Some(entry_point);
        }
    }
    None
}

/// The first Win32 entry point named on the Windows-only module in `source`.
///
/// Both documented spellings are handled: `from keypirinha_wintypes import NAME`
/// names an entry point directly, while `import keypirinha_wintypes [as ALIAS]`
/// binds a name whose `ALIAS.attribute` accesses are the entry points. The
/// availability probe is skipped either way.
fn windows_only_entry_point_in(source: &str) -> Option<String> {
    const PROBE: &str = "is_available";
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';

    let mut binding: Option<String> = None;
    for line in source.lines() {
        let statement = line.trim_start();
        if let Some(after) = statement
            .strip_prefix("from ")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix(WINDOWS_ONLY_MODULE))
        {
            if let Some(names) = after.trim_start().strip_prefix("import ") {
                if let Some(name) = names
                    .split(|c: char| !is_ident(c))
                    .find(|token| !token.is_empty() && *token != PROBE)
                {
                    return Some(name.to_owned());
                }
            }
        } else if let Some(rest) = statement.strip_prefix("import ") {
            let mut tokens = rest.split_whitespace();
            if tokens.next() == Some(WINDOWS_ONLY_MODULE) {
                binding = Some(match (tokens.next(), tokens.next()) {
                    (Some("as"), Some(alias)) => alias.to_owned(),
                    _ => WINDOWS_ONLY_MODULE.to_owned(),
                });
            }
        }
    }

    let binding = binding?;
    let needle = format!("{binding}.");
    for line in source.lines() {
        let mut from = 0;
        while let Some(relative) = line[from..].find(needle.as_str()) {
            let at = from + relative;
            let preceded_by_ident = line[..at].chars().next_back().is_some_and(is_ident);
            let attribute: String = line[at + needle.len()..]
                .chars()
                .take_while(|&c| is_ident(c))
                .collect();
            if !preceded_by_ident && !attribute.is_empty() && attribute != PROBE {
                return Some(attribute);
            }
            from = at + needle.len();
        }
    }
    None
}

/// Split the shim's `UndocumentedApiError` message into `(module, attribute)`.
///
/// The message is `"{module}.{attribute} is not part of the documented …"`
/// (`keypirinha.py`), so the subject is everything before ` is not part`, split
/// at its final `.`. Anything that does not fit that shape yields `None` rather
/// than a mislabelled finding.
fn split_undocumented(message: &str) -> Option<(String, String)> {
    let subject = message.split(" is not part").next().unwrap_or(message).trim();
    let (module, attribute) = subject.rsplit_once('.')?;
    if module.is_empty() || attribute.is_empty() {
        return None;
    }
    Some((module.to_owned(), attribute.to_owned()))
}

/// Whether `source` imports the Windows-only compatibility module.
///
/// Both documented spellings are matched, and only at statement level: a
/// mention of the name in a docstring or a comment is not a dependency, and
/// counting one would make every package that *documents* its portability
/// non-portable.
fn imports_windows_only(source: &str) -> bool {
    source.lines().any(|line| {
        let statement = line.trim_start();
        statement
            .strip_prefix("import ")
            .or_else(|| statement.strip_prefix("from "))
            .is_some_and(|rest| {
                rest.split(|character: char| !character.is_alphanumeric() && character != '_')
                    .next()
                    == Some(WINDOWS_ONLY_MODULE)
            })
    })
}

/// Records the two portability checks — the only ones whose presence depends on
/// what the package declares.
fn classify_portability(checks: &mut BTreeMap<&'static str, Check>, dependency: Option<&str>) {
    match dependency {
        Some(found) => {
            // Detecting the dependency is a static fact and works on every host,
            // so this passes anywhere: the package does declare what it needs.
            checks.insert(
                "windows_only_dependencies_declared",
                Check::pass(format!(
                    "the package declares a Windows-only dependency in its own source: {found}"
                )),
            );
            // Exercising Win32 is not a static fact. On a host without it there
            // is nothing to run, and saying so is the only honest answer
            // available (roadmap principle 7).
            checks.insert(
                WIN32_CHECK,
                if cfg!(windows) {
                    Check::pass(format!(
                        "the Win32 entry points behind {WINDOWS_ONLY_MODULE} are operational on \
                         this host"
                    ))
                } else {
                    Check {
                        result: CheckResult::Unavailable,
                        detail: format!(
                            "this host is {platform}, so the Win32 entry points behind \
                             {WINDOWS_ONLY_MODULE} cannot be exercised here; needing Windows is a \
                             portability fact, not a conformance failure",
                            platform = host_platform(),
                        ),
                    }
                },
            );
        }
        None => {
            checks.insert(
                "windows_only_dependencies_declared",
                Check::pass(
                    "no module in the package imports a Windows-only compatibility module, so it \
                     names no Win32 entry point"
                        .to_owned(),
                ),
            );
        }
    }
}

fn host_platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

// ---------------------------------------------------------------------------
// Check results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckResult {
    Pass,
    Fail,
    /// The check could not be run on this host. Distinct from `Pass` on
    /// purpose: a check backed by Win32 that "passed" on Linux would report
    /// coverage nobody ever had.
    Unavailable,
}

impl CheckResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug)]
struct Check {
    result: CheckResult,
    /// Why the check reached its result. Never empty, a pass included: a report
    /// is read by someone deciding a corpus classification, and "it passed"
    /// without saying what was observed is not evidence.
    detail: String,
}

impl Check {
    fn pass(detail: String) -> Self {
        Self {
            result: CheckResult::Pass,
            detail,
        }
    }

    fn fail(detail: String) -> Self {
        Self {
            result: CheckResult::Fail,
            detail,
        }
    }

    fn verdict(passed: bool, pass_detail: String, fail_detail: String) -> Self {
        if passed {
            Self::pass(pass_detail)
        } else {
            Self::fail(fail_detail)
        }
    }
}

// ---------------------------------------------------------------------------
// The worker handle the suite drives
// ---------------------------------------------------------------------------

/// What the suite observed crossing the worker boundary.
///
/// Bounded by construction: the dispatch log and the reply queue are driven by a
/// fixed timeline of a dozen intake calls, and the queue is drained after every
/// phase, so neither can hold more than the handful of frames one phase makes.
#[derive(Debug, Default)]
struct SuiteLog {
    /// `(timestamp, request)` for everything the runtime actually dispatched.
    dispatched: Vec<(Millis, LegacyRequest)>,
    /// Replies the child has produced and the driver has not yet fed back
    /// through `deliver`. Withholding them is what lets the runtime observe a
    /// genuinely in-flight callback even though the call itself is synchronous.
    replies: VecDeque<LegacyResponse>,
    /// The superseded on_suggest's cooperation, captured when its withheld reply
    /// is finally delivered (spec 9.2, acceptance 31.17): how many times it read
    /// `should_terminate()` and how many items it published after the host raised
    /// the flag. This is the obsolete work the termination check judges, never
    /// the fresh generation that replaced it.
    obsolete_polls: Option<u32>,
    obsolete_published: usize,
    /// Undocumented-API reaches the shim attributed to the plugin, as
    /// `(module, attribute)` (spec 14.12): a live feed for the §26.2 store.
    undocumented: Vec<(String, String)>,
    /// Cooperative termination requests the runtime raised.
    terminations: usize,
    /// Anything that went wrong on the boundary, reported through the checks it
    /// invalidates rather than swallowed.
    errors: Vec<String>,
}

/// The outbound half of the worker surface, backed by a real child process.
///
/// `dispatch` performs the blocking [`LegacyWorker::call`] there and then and
/// buffers the reply, so every callback the suite reports on genuinely ran in a
/// separate operating-system process (spec 4.2, 5.1). The reply is handed back
/// through [`LegacyRuntime::deliver`] at a virtual timestamp the driver chooses,
/// which keeps the scheduling half of the suite a pure state machine over an
/// explicit clock while the plugin half stays real.
#[derive(Debug)]
struct SuiteWorker {
    /// `None` once the runtime has stopped it. A dispatch afterwards would be a
    /// bug in the driver, and is recorded rather than papered over.
    worker: Option<LegacyWorker>,
    log: Rc<RefCell<SuiteLog>>,
}

impl LegacyWorkerHandle for SuiteWorker {
    fn dispatch(&mut self, at_ms: Millis, request: &LegacyRequest) -> Result<(), WorkerError> {
        self.log.borrow_mut().dispatched.push((at_ms, request.clone()));
        let Some(worker) = self.worker.as_mut() else {
            self.log
                .borrow_mut()
                .errors
                .push(format!("{:?} was dispatched after shutdown", request.callback()));
            return Ok(());
        };
        match worker.call(request.clone()) {
            Ok(response) => {
                self.log.borrow_mut().replies.push_back(response);
                Ok(())
            }
            Err(error) => {
                self.log
                    .borrow_mut()
                    .errors
                    .push(format!("{:?} failed: {error}", request.callback()));
                Err(error)
            }
        }
    }

    fn request_termination(
        &mut self,
        _at_ms: Millis,
        _plugin: &PluginId,
        _instance: InstanceId,
        _generation: Generation,
        _reason: TerminationReason,
    ) -> Result<(), WorkerError> {
        self.log.borrow_mut().terminations += 1;
        if let Some(worker) = self.worker.as_mut() {
            // Raising the flag is the host's job; honouring it is the plugin's,
            // and the suite reports on exactly that difference (spec 9.2).
            worker.terminate_handle().signal();
        }
        Ok(())
    }

    fn stop(&mut self, _at_ms: Millis, _budget_ms: Millis) -> Result<(), WorkerError> {
        if let Some(worker) = self.worker.take() {
            if let Err(error) = worker.shutdown() {
                self.log
                    .borrow_mut()
                    .errors
                    .push(format!("worker shutdown failed: {error}"));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The suite itself
// ---------------------------------------------------------------------------

/// One conformance run: the per-rule checks plus the compatibility observations
/// the run made for the §26.2 diagnostics store, which no other live path feeds
/// (Finding 5).
#[derive(Debug)]
struct ConformanceRun {
    checks: BTreeMap<&'static str, Check>,
    /// The superseded on_suggest's cooperation, when the run reached the
    /// termination phase: how long it ran after being asked to stop and whether
    /// it ever read `should_terminate()`.
    obsolete_callback: Option<CallbackObservation>,
    /// Undocumented-API reaches the shim attributed to the plugin (spec 14.12).
    undocumented: Vec<(String, String)>,
}

impl ConformanceRun {
    /// A run that could not proceed: every rule reported, no observations made.
    fn only_checks(checks: BTreeMap<&'static str, Check>) -> Self {
        Self {
            checks,
            obsolete_callback: None,
            undocumented: Vec::new(),
        }
    }
}

/// Runs the whole conformance timeline and returns one result per rule.
///
/// Written as one linear scenario rather than one scenario per check, because
/// the summary must describe *the same run* as the detail: thirteen independent
/// runs could each be internally consistent and still add up to a report of a
/// package that was never in any of those states.
fn run_conformance(package: &LegacyPackage, interpreter: &Interpreter) -> ConformanceRun {
    let mut checks: BTreeMap<&'static str, Check> = BTreeMap::new();
    let plugin = plugin_of(package);

    let options = match worker_options(&plugin) {
        Ok(options) => options,
        Err(message) => return ConformanceRun::only_checks(unrunnable(checks, &message)),
    };
    let worker = match LegacyWorker::spawn(interpreter, package, options) {
        Ok(worker) => worker,
        Err(error) => {
            return ConformanceRun::only_checks(unrunnable(
                checks,
                &format!("the legacy worker did not start: {error}"),
            ))
        }
    };

    // The process id is compared and deliberately never printed: it changes
    // between runs, and a report that changed between runs could not be diffed.
    let in_child_process = worker.process_id() != std::process::id();
    checks.insert(
        "worker_runs_out_of_process",
        Check::verdict(
            in_child_process,
            "the plugin's callbacks ran in a child interpreter process, never in the host \
             process (spec 4.2, 5.1)"
                .to_owned(),
            "the worker reported the host's own process id, so plugin code ran in the CriKey \
             process (spec 4.2)"
                .to_owned(),
        ),
    );

    let log = Rc::new(RefCell::new(SuiteLog::default()));
    let mut runtime = LegacyRuntime::new(
        SuiteWorker {
            worker: Some(worker),
            log: Rc::clone(&log),
        },
        deadlines(),
    );
    let mut driver = Driver {
        plugin: plugin.clone(),
        log: Rc::clone(&log),
    };

    let original = runtime.register(plugin.clone(), package.id.clone());

    // -- one-time initialization (spec 14.8) -------------------------------
    let booted = runtime.tick(T_BOOT).len();
    let boot_deliveries = driver.settle(&mut runtime, T_BOOT).len();
    if booted == 0 || boot_deliveries == 0 {
        // The child is reaped even on the path that gives up, so a developer
        // command can never leave an orphaned interpreter behind (spec 24.3).
        let _ = runtime.shutdown(T_BOOT);
        return ConformanceRun::only_checks(unrunnable(
            checks,
            "the plugin's one-time on_start never crossed the worker boundary",
        ));
    }

    // -- repeated on_catalog (spec 14.8) -----------------------------------
    let mut rebuilds_accepted = 0_usize;
    for at_ms in [T_CATALOG_FIRST, T_CATALOG_SECOND] {
        if let Err(error) = runtime.catalog_rebuild(&plugin, at_ms) {
            driver.note(format!("catalog_rebuild at {at_ms} was refused: {error:?}"));
            continue;
        }
        runtime.tick(at_ms);
        if driver
            .settle(&mut runtime, at_ms)
            .iter()
            .any(|delivery| matches!(delivery, Delivery::CatalogUpdated { .. }))
        {
            rebuilds_accepted += 1;
        }
    }
    let counted_rebuilds = runtime
        .diagnostics(&plugin)
        .map_or(0, |counters| counters.catalog_rebuilds);
    checks.insert(
        "repeated_on_catalog_permitted",
        Check::verdict(
            rebuilds_accepted == 2 && counted_rebuilds == 2,
            "on_catalog() ran twice on the live instance and both complete publications were \
             accepted, while the one-time on_start ran exactly once"
                .to_owned(),
            format!(
                "spec 14.8 permits repeated on_catalog(), but only {rebuilds_accepted} of 2 \
                 rebuilds were accepted ({counted_rebuilds} counted)"
            ),
        ),
    );

    // -- broadcast, no gating, no debounce (spec 14.5, 8.10, 8.11) ---------
    let empty_generation = runtime.submit_query("", T_QUERY_EMPTY);
    runtime.tick(T_QUERY_EMPTY);
    driver.settle(&mut runtime, T_QUERY_EMPTY);
    let empty_dispatch = driver.dispatch_of(&LegacyRequestKind::InitialSuggest { query: String::new() });
    let recipients = runtime.trace().iter().find_map(|event| match event {
        LegacyTraceEvent::Broadcast {
            at_ms,
            generation,
            plugins,
        } if *at_ms == T_QUERY_EMPTY && *generation == empty_generation => Some(plugins.len()),
        _ => None,
    });
    checks.insert(
        "initial_query_broadcast",
        Check::verdict(
            empty_dispatch == Some(T_QUERY_EMPTY) && recipients.is_some(),
            format!(
                "the initial suggestion request was broadcast to every loaded legacy plugin \
                 ({count} recipient(s)) and reached this one as on_suggest with no selected item",
                count = recipients.unwrap_or(0),
            ),
            "the initial suggestion request never reached the plugin as a broadcast on_suggest \
             (spec 14.5, acceptance 31.15)"
                .to_owned(),
        ),
    );
    checks.insert(
        "host_gating_disabled",
        Check::verdict(
            !SchedulingProfile::LegacyStrict.allows_host_gating() && empty_dispatch.is_some(),
            "the empty query was dispatched verbatim: legacy-strict imposes no minimum query \
             length and no prefix or keyword relevance gating"
                .to_owned(),
            "the empty query was suppressed, so a host gate narrowed a legacy broadcast \
             (spec 8.11, 14.5)"
                .to_owned(),
        ),
    );

    // Two further keystrokes one millisecond apart, each answered before the
    // next arrives, so nothing observed here can be attributed to serialization
    // rather than to debouncing.
    let mut punctual = vec![empty_dispatch == Some(T_QUERY_EMPTY)];
    let mut payloads: Vec<(&str, Vec<Published>)> = Vec::new();
    for (at_ms, query) in [(T_QUERY_ALPHA, QUERY_ALPHA), (T_QUERY_BETA, QUERY_BETA)] {
        runtime.submit_query(query, at_ms);
        runtime.tick(at_ms);
        let published = driver.settle_and_collect(&mut runtime, at_ms);
        punctual.push(
            driver.dispatch_of(&LegacyRequestKind::InitialSuggest {
                query: query.to_owned(),
            }) == Some(at_ms),
        );
        payloads.push((query, published));
    }
    checks.insert(
        "host_time_debounce_disabled",
        Check::verdict(
            !SchedulingProfile::LegacyStrict.allows_time_debounce()
                && punctual.iter().all(|dispatched| *dispatched),
            format!(
                "three consecutive keystrokes at {T_QUERY_EMPTY}, {T_QUERY_ALPHA} and \
                 {T_QUERY_BETA} ms each reached the idle plugin at its own timestamp, never at \
                 the end of a debounce interval"
            ),
            "a keystroke did not reach the idle legacy plugin at its own timestamp, so it was \
             time-debounced (spec 8.4, acceptance 31.14)"
                .to_owned(),
        ),
    );

    // -- dynamic suggestions are recomputed per request (spec 14.9) --------
    checks.insert("dynamic_suggestions_not_cached", caching_check(&payloads));

    // -- routing after a selection (spec 14.5) -----------------------------
    let selected = payloads
        .last()
        .and_then(|(_, published)| published.first().map(|item| item.identity.clone()))
        .or_else(|| {
            runtime
                .catalog(&plugin)
                .first()
                .map(|item| item.stable_id.0.clone())
        });
    checks.insert(
        "selected_item_routed_to_owner",
        match selected {
            None => Check::fail(
                "the plugin published neither a suggestion nor a catalog item, so no selection \
                 could be routed to it"
                    .to_owned(),
            ),
            Some(identity) => {
                let item_id = ItemId(identity);
                match runtime.select_item(&item_id, T_SELECT) {
                    Err(error) => {
                        Check::fail(format!("selecting the plugin's own item was refused: {error:?}"))
                    }
                    Ok(generation) => {
                        runtime.tick(T_SELECT);
                        let routed = driver.dispatched_at(T_SELECT);
                        let to_owner = routed.len() == 1
                            && routed.iter().any(|request| {
                                matches!(
                                    &request.kind,
                                    LegacyRequestKind::ArgumentSuggest { selected, .. }
                                        if selected == &item_id
                                )
                            });
                        let recorded = runtime.trace().iter().any(|event| {
                            matches!(
                                event,
                                LegacyTraceEvent::Routed {
                                    at_ms,
                                    generation: traced,
                                    plugin: owner,
                                    owner_of,
                                } if *at_ms == T_SELECT
                                    && *traced == generation
                                    && owner == &plugin
                                    && owner_of == &item_id
                            )
                        });
                        driver.settle(&mut runtime, T_ARGUMENT_ANSWER);
                        Check::verdict(
                            to_owner && recorded,
                            "once an item was selected the suggestion request was routed to the \
                             plugin that owns it, carrying the selected item, rather than \
                             broadcast"
                                .to_owned(),
                            "the argument-suggestion request after a selection did not reach the \
                             owning plugin as a routed request (spec 14.5)"
                                .to_owned(),
                        )
                    }
                }
            }
        },
    );

    // -- serialization, replacement, staleness -----------------------------
    // The reply to this request is deliberately withheld, so the runtime
    // observes a callback that is genuinely still in flight.
    let obsolete_generation = runtime.submit_query(QUERY_SERIAL_FIRST, T_SERIAL_DISPATCHED);
    runtime.tick(T_SERIAL_DISPATCHED);
    let in_flight = driver.dispatched_at(T_SERIAL_DISPATCHED).len();

    let superseding = runtime.submit_query(QUERY_SERIAL_SECOND, T_SERIAL_SECOND);
    let while_busy_second = runtime.tick(T_SERIAL_SECOND).len();
    let newest = runtime.submit_query(QUERY_SERIAL_THIRD, T_SERIAL_THIRD);
    let while_busy_third = runtime.tick(T_SERIAL_THIRD).len();

    let busy_state = runtime.instance_state(&plugin);
    checks.insert(
        "callbacks_serialized_per_instance",
        Check::verdict(
            in_flight == 1
                && while_busy_second == 0
                && while_busy_third == 0
                && busy_state
                    .as_ref()
                    .is_some_and(|state| state.pending_depth == 1 && state.running.is_some()),
            "while one legacy callback was in flight no second callback started on the same \
             instance; the newer keystrokes waited and exactly one request was retained"
                .to_owned(),
            format!(
                "two callbacks were allowed to overlap on one legacy instance, or the pending \
                 queue grew past one: {busy_state:?} (spec 14.5, acceptance 31.16)"
            ),
        ),
    );

    let replaced = runtime
        .diagnostics(&plugin)
        .map_or(0, |counters| counters.replaced);
    let retained = busy_state.as_ref().and_then(|state| state.pending);
    checks.insert(
        "obsolete_work_replaced",
        Check::verdict(
            replaced >= 1 && retained == Some(newest) && superseding != newest,
            "the intermediate undispatched query was replaced by the newest one rather than \
             queued behind it, so only the newest survived to be dispatched"
                .to_owned(),
            format!(
                "obsolete undispatched work was not replaced: {replaced} replacement(s) recorded \
                 and the retained request was {retained:?} rather than the newest \
                 (spec 8.4, 8.8, 14.5)"
            ),
        ),
    );

    // The withheld answer arrives long after its query stopped being visible.
    let late = driver.settle_recording_obsolete(&mut runtime, T_STALE_ANSWER);
    let counted_stale = runtime
        .diagnostics(&plugin)
        .map_or(0, |counters| counters.stale_rejected);
    checks.insert(
        "stale_results_rejected",
        Check::verdict(
            late.iter()
                .any(|delivery| matches!(delivery, Delivery::RejectedStale { .. }))
                && counted_stale >= 1,
            "an answer for a superseded query generation was rejected at the intake boundary, \
             however long after the fact it arrived, and changed nothing displayed"
                .to_owned(),
            format!(
                "a superseded generation's answer was not rejected: {late:?} ({counted_stale} \
                 counted) (spec 8.5, acceptance 31.7)"
            ),
        ),
    );

    // -- cooperative termination on the SUPERSEDED work (spec 9.2, 14.5;
    //    acceptance 31.17) -------------------------------------------------
    // The verdict is about the obsolete generation the host actually asked to
    // stop, never the fresh generation that replaced it: once termination is
    // lowered for fresh work (Finding 1), the retained newest query correctly
    // observes `should_terminate() == false`, so judging it would test the
    // opposite of the rule. The superseded work's own reply — captured when its
    // withheld answer was delivered above — is the evidence: a cooperative
    // plugin reads the flag (poll count > 0), an uncooperative one never does,
    // and whatever the obsolete work published was rejected as stale, so the
    // display never depended on the plugin's cooperation.
    runtime.tick(T_RESUME);
    let _ = driver.settle(&mut runtime, T_RESUME_ANSWER); // drain the retained fresh work
    let raised = driver.terminations();
    let raised_for_obsolete = runtime.trace().iter().any(|event| {
        matches!(
            event,
            LegacyTraceEvent::TerminationRequested {
                plugin: owner,
                generation,
                reason,
                ..
            } if owner == &plugin
                && *generation == obsolete_generation
                && *reason == TerminationReason::QuerySuperseded
        )
    });
    let obsolete_polls = driver.obsolete_polls();
    let obsolete_published = driver.obsolete_published();
    let obsolete_rejected = late
        .iter()
        .any(|delivery| matches!(delivery, Delivery::RejectedStale { .. }));
    checks.insert(
        "should_terminate_observed",
        Check::verdict(
            raised >= 1
                && raised_for_obsolete
                && obsolete_polls.is_some_and(|count| count > 0)
                && obsolete_rejected,
            format!(
                "the host raised the cooperative flag {raised} time(s), once for the superseded \
                 query generation; that obsolete on_suggest read should_terminate() {count} \
                 time(s), and the {obsolete_published} item(s) it went on to publish were rejected \
                 as stale, so nothing it did after being superseded reached the display",
                count = obsolete_polls.unwrap_or(0),
            ),
            format!(
                "the host raised the cooperative flag {raised} time(s) for the superseded query \
                 generation, yet that obsolete on_suggest read should_terminate() {count} time(s) \
                 before publishing {obsolete_published} item(s); a legacy callback that never \
                 consults the flag cannot cooperate with termination (spec 9.2, acceptance 31.17)",
                count = obsolete_polls.unwrap_or(0),
            ),
        ),
    );
    // Fed to the §26.2 diagnostics store by the caller: a superseded callback
    // that never polled is precisely spec 26.2's "long callback that does not
    // check should_terminate()".
    let obsolete_callback = obsolete_polls.map(|count| CallbackObservation {
        duration_ms: T_STALE_ANSWER.saturating_sub(T_SERIAL_SECOND),
        observed_should_terminate: count > 0,
    });

    // -- catalog updates from a superseded instance (spec 14.8) ------------
    let orphaned = match runtime.catalog_rebuild(&plugin, T_ORPHAN_BUILD) {
        Ok(()) => {
            runtime.tick(T_ORPHAN_BUILD);
            driver.dispatched_at(T_ORPHAN_BUILD).len()
        }
        Err(error) => {
            driver.note(format!("the final catalog_rebuild was refused: {error:?}"));
            0
        }
    };
    let replacement = runtime.reload(&plugin, T_RELOAD);
    let orphan_delivery = driver.settle(&mut runtime, T_ORPHAN_ANSWER);
    let counted_rejections = runtime
        .diagnostics(&plugin)
        .map_or(0, |counters| counters.catalog_updates_rejected);
    checks.insert(
        "obsolete_catalog_updates_rejected",
        Check::verdict(
            orphaned == 1
                && replacement.as_ref().is_ok_and(|new| new != &original)
                && orphan_delivery
                    .iter()
                    .any(|delivery| matches!(delivery, Delivery::RejectedObsoleteInstance { .. }))
                && counted_rejections >= 1,
            "a catalog published by a plugin instance the reload had superseded was rejected, \
             leaving the live catalog unmutated"
                .to_owned(),
            format!(
                "a superseded instance's catalog was not rejected: reload gave {replacement:?} \
                 and the delivery was {orphan_delivery:?} ({counted_rejections} counted) \
                 (spec 14.8)"
            ),
        ),
    );

    let _ = runtime.shutdown(T_SHUTDOWN);

    // Anything that went wrong on the boundary is attached to the checks it
    // invalidates rather than reported as a blanket failure: a suite that
    // answers "everything failed" sends the maintainer back to the spec to
    // guess which rule the package actually broke.
    let boundary = log.borrow().errors.join("; ");
    if !boundary.is_empty() {
        for check in checks.values_mut() {
            if check.result == CheckResult::Fail {
                check.detail.push_str(&format!(" [worker boundary: {boundary}]"));
            }
        }
    }

    // Bound the borrow before the block ends: `log` is dropped with the other
    // locals, and a `Ref` living into the tail expression would outlive it.
    let undocumented = log.borrow().undocumented.clone();
    ConformanceRun {
        checks,
        obsolete_callback,
        undocumented,
    }
}

/// Compares the payloads two distinct queries produced.
///
/// A cached dynamic answer is indistinguishable from a stale one (spec 8.5) and
/// the user cannot tell which they are looking at, so spec 14.9 and acceptance
/// 31.18 forbid it under the default profile. Two *different* queries answering
/// with a byte-identical payload is what a memo keyed on anything but the query
/// looks like from outside the plugin.
fn caching_check(payloads: &[(&str, Vec<Published>)]) -> Check {
    let (Some((first_query, first)), Some((second_query, second))) = (payloads.first(), payloads.last())
    else {
        return Check::fail(
            "the suite obtained no query answers, so the no-caching rule of spec 14.9 was not \
             exercised"
                .to_owned(),
        );
    };
    if first_query == second_query {
        return Check::fail(
            "the suite could not obtain two distinct query answers to compare, so the no-caching \
             rule of spec 14.9 was not exercised"
                .to_owned(),
        );
    }

    if first.is_empty() && second.is_empty() {
        // Publishing nothing is not a cache: there is no dynamic answer to
        // outlive its query. A plugin whose whole offering is static is
        // conformant here by construction.
        return Check::pass(
            "the plugin publishes no dynamic suggestions, so no answer can outlive the query \
             that produced it"
                .to_owned(),
        );
    }

    Check::verdict(
        !SchedulingProfile::LegacyStrict.allows_dynamic_result_cache() && first != second,
        format!(
            "the answers to `{first_query}` and `{second_query}` differ, so the payload was \
             recomputed from the query rather than served from a cache; legacy-strict itself \
             forbids a dynamic result cache"
        ),
        format!(
            "`{first_query}` and `{second_query}` produced a byte-identical payload, so a dynamic \
             answer outlived the query that produced it (spec 14.9, acceptance 31.18)"
        ),
    )
}

/// Every core check reported as failing for the one reason the suite could not
/// get past.
///
/// Used only when the run could not start. Every check still appears, so the
/// summary counts and the detail lines describe the same failed run rather than
/// the report going silent on twelve rules.
fn unrunnable(mut checks: BTreeMap<&'static str, Check>, reason: &str) -> BTreeMap<&'static str, Check> {
    for name in CORE_CHECKS {
        checks
            .entry(name)
            .or_insert_with(|| Check::fail(format!("the conformance run could not start: {reason}")));
    }
    checks
}

// ---------------------------------------------------------------------------
// Driving the runtime
// ---------------------------------------------------------------------------

/// One published item projected onto comparable, printable scalars.
///
/// `Item` is deliberately not comparable in the core, and the stable id alone
/// would not be enough anyway: a plugin serving a memo keyed on anything but the
/// query republishes the same id *and* the same label, and it is the two
/// together that make two answers byte-identical rather than merely similar.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Published {
    identity: String,
    projection: String,
}

fn project(item: &Item) -> Published {
    Published {
        identity: item.stable_id.0.clone(),
        projection: format!(
            "{id}\u{1f}{label}\u{1f}{target}\u{1f}{description}",
            id = item.stable_id.0,
            label = item.label,
            target = item.target,
            description = item.description,
        ),
    }
}

/// Feeds buffered child replies back into the runtime and reads the dispatch
/// log. Holds no state of its own beyond the shared log.
#[derive(Debug)]
struct Driver {
    plugin: PluginId,
    log: Rc<RefCell<SuiteLog>>,
}

impl Driver {
    fn note(&mut self, message: String) {
        self.log.borrow_mut().errors.push(message);
    }

    /// The next reply the child has produced.
    ///
    /// Split out so the `RefCell` borrow ends before `deliver` runs: the runtime
    /// re-enters the handle, which borrows the same log, and a borrow held
    /// across the call would panic.
    fn next_reply(&self) -> Option<LegacyResponse> {
        let response = self.log.borrow_mut().replies.pop_front()?;
        // A live feed for the §26.2 undocumented-API diagnostic: the shim raises
        // `UndocumentedApiError` for a reach outside the documented surface
        // (`keypirinha.py`) and it arrives here as a healthy-worker failure whose
        // message names `module.attribute` (spec 14.12).
        if let LegacyOutcome::Failed(exception) = &response.outcome {
            if exception.exception_type == "UndocumentedApiError" {
                if let Some(access) = split_undocumented(&exception.message) {
                    self.log.borrow_mut().undocumented.push(access);
                }
            }
        }
        Some(response)
    }

    /// Delivers every reply the child has produced, at `at_ms`.
    fn settle(&mut self, runtime: &mut LegacyRuntime<SuiteWorker>, at_ms: Millis) -> Vec<Delivery> {
        let mut deliveries = Vec::new();
        while let Some(response) = self.next_reply() {
            deliveries.push(runtime.deliver(response, at_ms));
        }
        deliveries
    }

    /// Delivers every buffered reply at `at_ms`, recording the SUPERSEDED work's
    /// cooperation for the termination check and the §26.2 diagnostics: the
    /// termination polls its callback counted and how many items it published
    /// after the host raised the flag (spec 9.2, acceptance 31.17). Only the
    /// obsolete reply is pending when this runs, so the record can never be
    /// attributed to the fresh generation that replaced it.
    fn settle_recording_obsolete(
        &mut self,
        runtime: &mut LegacyRuntime<SuiteWorker>,
        at_ms: Millis,
    ) -> Vec<Delivery> {
        let mut deliveries = Vec::new();
        while let Some(response) = self.next_reply() {
            {
                let mut log = self.log.borrow_mut();
                log.obsolete_polls = Some(response.terminate_polls);
                log.obsolete_published += published_items(&response.outcome).map_or(0, <[Item]>::len);
            }
            deliveries.push(runtime.deliver(response, at_ms));
        }
        deliveries
    }

    /// Delivers every reply and returns what the plugin published.
    fn settle_and_collect(
        &mut self,
        runtime: &mut LegacyRuntime<SuiteWorker>,
        at_ms: Millis,
    ) -> Vec<Published> {
        let mut published = Vec::new();
        while let Some(response) = self.next_reply() {
            if let Some(items) = published_items(&response.outcome) {
                published.extend(items.iter().map(project));
            }
            let _ = runtime.deliver(response, at_ms);
        }
        published
    }

    /// The timestamp `kind` was dispatched to this plugin at, if it was.
    ///
    /// Compared through `Debug` rather than `PartialEq`: `LegacyRequestKind`
    /// carries `Item` values for `on_execute` and the core deliberately does not
    /// make `Item` comparable, so the kinds cannot be compared directly.
    fn dispatch_of(&self, kind: &LegacyRequestKind) -> Option<Millis> {
        let wanted = format!("{kind:?}");
        self.log
            .borrow()
            .dispatched
            .iter()
            .find(|(_, request)| request.plugin == self.plugin && format!("{:?}", request.kind) == wanted)
            .map(|(at_ms, _)| *at_ms)
    }

    fn dispatched_at(&self, at_ms: Millis) -> Vec<LegacyRequest> {
        self.log
            .borrow()
            .dispatched
            .iter()
            .filter(|(when, request)| *when == at_ms && request.plugin == self.plugin)
            .map(|(_, request)| request.clone())
            .collect()
    }

    /// The superseded on_suggest's cooperation: the polls its callback counted
    /// and how many items it published after the host raised the flag. `None`
    /// polls until the obsolete reply has been delivered (spec 9.2).
    fn obsolete_polls(&self) -> Option<u32> {
        self.log.borrow().obsolete_polls
    }

    fn obsolete_published(&self) -> usize {
        self.log.borrow().obsolete_published
    }

    /// Cooperative termination requests the runtime raised on this run. Raising
    /// the flag is the host's half of spec 9.2, and a report that only counted
    /// the plugin's polls could not tell a cooperative plugin from a host that
    /// never asked it to stop.
    fn terminations(&self) -> usize {
        self.log.borrow().terminations
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// One summary line. Every key is written once per run, so a reader never has to
/// decide which of two values describes the run.
fn field(out: &mut String, key: &str, value: &str) {
    writeln!(out, "{key}={}", encode(value)).expect("writing to a String cannot fail");
}

/// Feed every compatibility observation the command can genuinely make about
/// `plugin` into a fresh diagnostics store (spec 26.1, 26.2, 14.11, 14.12;
/// acceptance 31.29, 31.31). This is the live feed the §26.2 subsystem otherwise
/// lacks (Finding 5); every observation is a fact the command already
/// established, so the store stays deterministic on every host.
fn compatibility_diagnostics(
    plugin: &PluginId,
    package: &LegacyPackage,
    interpreter: &Interpreter,
    dependency: Option<&str>,
    declared: Option<PluginClassification>,
    obsolete_callback: Option<CallbackObservation>,
    undocumented: &[(String, String)],
) -> LegacyDiagnostics {
    // The reportable long callback here is one that answered a superseded request
    // without ever polling should_terminate(); the threshold is zero because the
    // defect is the missing poll, not the duration spec 9.6 forbids acting on.
    let mut diagnostics = LegacyDiagnostics::with_limits(DiagnosticLimits {
        long_callback_threshold_ms: 0,
        ..DiagnosticLimits::default()
    });

    // Scheduling profile (spec 26.2): a fact about how CriKey runs the plugin,
    // not a defect.
    diagnostics.observe_scheduling_profile(plugin, SchedulingProfile::LegacyStrict);

    // Imports (spec 14.2, 14.12; acceptance 31.31): a Windows-only module makes
    // the plugin non-portable wherever it runs, detected from the package's own
    // source rather than from a self-report.
    if let Some(detail) = dependency {
        let entry_point =
            scan_windows_only_entry_point(package).unwrap_or_else(|| "unnamed Win32 entry point".to_owned());
        diagnostics.observe_import(
            plugin,
            WINDOWS_ONLY_MODULE,
            ImportOutcome::WindowsOnly {
                entry_point,
                detail: detail.to_owned(),
            },
        );
    }

    // The published classification (spec 27.4; acceptance 31.31). The corpus
    // documents limitations no scan of this host can reproduce — a package that
    // drives Core Audio through `comtypes`, or one blocked on APIs CriKey has
    // not shipped — so without this the store would answer "portable" for a
    // package the project itself has published as anything but. A portable
    // classification, and a package the corpus does not reference, file
    // nothing.
    if let Some(classification) = declared {
        diagnostics.observe_declared_classification(plugin, classification);
    }

    // Python version (spec 14.11): a legacy package declares no interpreter
    // requirement, so this verifies the resolved interpreter meets the layer's
    // supported floor; a supported host is clean and reports nothing.
    diagnostics.observe_python_requirement(plugin, MINIMUM_SUPPORTED_PYTHON, interpreter.version());

    // A long callback that never read should_terminate() on superseded work
    // (spec 9.2, 9.6, 26.2). Absent for a run that never reached the phase.
    if let Some(observation) = obsolete_callback {
        diagnostics.observe_callback(plugin, LegacyCallback::OnSuggest, observation);
    }

    // Undocumented API access, exactly as the shim reported it (spec 14.12).
    for (module, symbol) in undocumented {
        diagnostics.observe_api_access(plugin, module, symbol, None);
    }

    diagnostics
}

/// Render every finding held for `plugin` as `key=value` summary lines (spec
/// 26.1, 26.2). One line per finding, its stable code embedded in every key so
/// the lines stay unique across the run, carrying the severity, the prose, and —
/// where spec 26.2 requires one — the suggested source change, each value
/// percent-encoded like every other. First-occurrence order keeps two runs of
/// one invocation byte-identical.
fn render_diagnostics(out: &mut String, diagnostics: &LegacyDiagnostics, plugin: &PluginId) {
    for record in diagnostics.warnings_for(plugin) {
        let warning = &record.warning;
        let code = warning.code();
        write!(
            out,
            "warning.{code}.severity={}",
            encode(warning.severity().as_str())
        )
        .expect("writing to a String cannot fail");
        write!(out, " warning.{code}.message={}", encode(&warning.message()))
            .expect("writing to a String cannot fail");
        if let Some(suggestion) = warning.suggestion() {
            write!(out, " warning.{code}.suggestion={}", encode(&suggestion))
                .expect("writing to a String cannot fail");
        }
        out.push('\n');
    }
}

/// One catalog item, carrying every field of spec 10.1.
fn item_line(index: usize, item: &Item) -> String {
    let mut line = String::new();
    for (key, value) in [
        ("item", index.to_string()),
        ("id", item.stable_id.0.clone()),
        ("category", item.category.as_str().to_owned()),
        ("label", item.label.clone()),
        ("description", item.description.clone()),
        ("target", item.target.clone()),
        ("search_terms", item.search_terms.len().to_string()),
        (
            "argument_policy",
            argument_policy(item.argument_policy).to_owned(),
        ),
        ("hit_policy", hit_policy(item.hit_policy).to_owned()),
        ("score_hint", item.score_hint.to_string()),
        ("actions", item.actions.len().to_string()),
    ] {
        if !line.is_empty() {
            line.push(' ');
        }
        write!(line, "{key}={}", encode(&value)).expect("writing to a String cannot fail");
    }
    line.push('\n');
    line
}

fn argument_policy(policy: ArgumentPolicy) -> &'static str {
    match policy {
        ArgumentPolicy::Forbidden => "forbidden",
        ArgumentPolicy::Optional => "optional",
        ArgumentPolicy::Required => "required",
    }
}

fn hit_policy(policy: HitPolicy) -> &'static str {
    match policy {
        HitPolicy::Recorded => "recorded",
        HitPolicy::Ignored => "ignored",
    }
}

/// Percent-encodes a value so a line always splits into `key=value` tokens.
///
/// Only unreserved ASCII survives unescaped. Everything else — the space that
/// would split a token, the `=` that would split a pair, the `%` that would make
/// an escape ambiguous, and every non-ASCII byte — becomes `%XX` with uppercase
/// hex. One spelling per escape, because two encoders that disagreed on case
/// would produce two reports that diff against each other for no reason.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

/// The workspace this build came from.
///
/// A `crikey dev` command reads version-controlled repository data by definition
/// (spec 26.3, 28), so the root is the one this binary was built in rather than
/// whatever directory it happens to be run from: a developer command whose
/// counts depended on the shell's working directory could not be compared
/// between two people.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_character_that_would_break_the_format_is_escaped_with_uppercase_hex() {
        assert_eq!(
            encode("Deterministic Fixture Item #2 (50% = half)"),
            "Deterministic%20Fixture%20Item%20%232%20%2850%25%20%3D%20half%29"
        );
        // Unreserved ASCII survives, so a path stays readable without decoding.
        assert_eq!(
            encode("compatibility/api-matrix/matrix.toml"),
            "compatibility/api-matrix/matrix.toml"
        );
        assert_eq!(encode(""), "");
    }

    #[test]
    fn a_non_ascii_label_round_trips_through_the_encoding() {
        let encoded = encode("Ünïcøde");
        assert!(
            encoded.bytes().all(|byte| byte.is_ascii()),
            "the encoding must produce ASCII, or the line is not byte-splittable: {encoded}"
        );
        assert!(encoded.contains("%C3%9C"), "{encoded}");
    }

    #[test]
    fn only_a_statement_level_import_declares_a_windows_only_dependency() {
        assert!(imports_windows_only("import keypirinha_wintypes as kpwt\n"));
        assert!(imports_windows_only(
            "from keypirinha_wintypes import declare_func\n"
        ));
        assert!(imports_windows_only("    import keypirinha_wintypes\n"));
        // Documenting portability must never make a package non-portable.
        assert!(!imports_windows_only(
            "\"\"\"This plugin never touches keypirinha_wintypes.\"\"\"\n"
        ));
        assert!(!imports_windows_only("# keypirinha_wintypes is not used\n"));
        assert!(!imports_windows_only("import keypirinha as kp\n"));
        assert!(!imports_windows_only("import keypirinha_wintypes_helper\n"));
    }

    #[test]
    fn help_is_asked_for_by_either_spelling_and_never_by_a_package_path() {
        assert!(wants_help(&["--help".to_owned()]));
        assert!(wants_help(&["-h".to_owned()]));
        assert!(wants_help(&[
            "--package".to_owned(),
            "/nowhere".to_owned(),
            "--help".to_owned(),
        ]));
        assert!(!wants_help(&["--package".to_owned(), "/nowhere".to_owned()]));
    }

    #[test]
    fn an_unusable_package_argument_list_is_refused_rather_than_guessed() {
        let refused: [Vec<String>; 5] = [
            vec![],
            vec!["--package".to_owned()],
            vec!["--package".to_owned(), String::new()],
            vec!["--package=".to_owned()],
            vec!["/some/package".to_owned()],
        ];
        for args in refused {
            assert!(
                parse_package_args("test-legacy-compat", &args).is_err(),
                "{args:?} names no usable package and must be refused"
            );
        }

        assert_eq!(
            parse_package_args("inspect-catalog", &["--package=/pkg".to_owned()]),
            Ok(Some("/pkg".to_owned()))
        );
        assert_eq!(
            parse_package_args("inspect-catalog", &["--package".to_owned(), "/pkg".to_owned()]),
            Ok(Some("/pkg".to_owned()))
        );
        // Help wins over validation: `--help` beside a path that could not be
        // loaded must still explain the command.
        assert_eq!(
            parse_package_args(
                "inspect-catalog",
                &["--package".to_owned(), String::new(), "--help".to_owned()]
            ),
            Ok(None)
        );
    }

    #[test]
    fn the_report_command_takes_no_arguments_but_still_explains_itself() {
        assert_eq!(parse_no_arguments("compatibility-report", &[]), Ok(true));
        assert_eq!(
            parse_no_arguments("compatibility-report", &["--help".to_owned()]),
            Ok(false)
        );
        assert!(parse_no_arguments("compatibility-report", &["everything".to_owned()]).is_err());
        assert!(parse_no_arguments(
            "compatibility-report",
            &["--package".to_owned(), "/pkg".to_owned()]
        )
        .is_err());
    }

    #[test]
    fn help_explains_a_command_without_emitting_a_report_line() {
        for text in [
            package_help("test-legacy-compat", CONFORMANCE_SYNOPSIS),
            package_help("inspect-catalog", CATALOG_SYNOPSIS),
            report_help("compatibility-report"),
        ] {
            assert!(text.contains("USAGE"), "{text}");
            for line in text.lines() {
                let first = line.split_whitespace().next().unwrap_or_default();
                for marker in ["check=", "item=", "verdict=", "matrix_apis="] {
                    assert!(
                        !first.starts_with(marker),
                        "help emitted a `{marker}` report line: {line}"
                    );
                }
            }
        }
        assert!(package_help("inspect-catalog", CATALOG_SYNOPSIS).contains("--package"));
    }

    #[test]
    fn a_summary_line_is_one_key_and_one_single_spaced_value() {
        let mut out = String::new();
        field(&mut out, "package", "/tmp/a package/well-behaved");
        assert_eq!(out, "package=/tmp/a%20package/well-behaved\n");
        assert_eq!(out.trim_end().split(' ').count(), 1);
    }

    #[test]
    fn the_verdict_vocabulary_and_the_check_roster_stay_in_step() {
        assert_eq!(CORE_CHECKS.len(), 13, "spec 14.5 / 14.8 / 14.9 / 14.12");
        let mut names = CORE_CHECKS;
        names.sort_unstable();
        let mut deduplicated = names.to_vec();
        deduplicated.dedup();
        assert_eq!(
            deduplicated.len(),
            CORE_CHECKS.len(),
            "a check reported twice makes its result ambiguous"
        );
        assert!(
            CORE_CHECKS.iter().all(|name| !name.is_empty()),
            "a nameless check cannot be acted on"
        );
        assert!(
            !CORE_CHECKS.contains(&WIN32_CHECK),
            "the Win32 check is conditional on a declared dependency and is never a core check"
        );
        assert_eq!(CheckResult::Pass.as_str(), "pass");
        assert_eq!(CheckResult::Fail.as_str(), "fail");
        assert_eq!(CheckResult::Unavailable.as_str(), "unavailable");
    }

    #[test]
    fn a_byte_identical_payload_for_two_different_queries_is_reported_as_caching() {
        let answer = |text: &str| {
            vec![Published {
                identity: "one".to_owned(),
                projection: text.to_owned(),
            }]
        };

        assert_eq!(
            caching_check(&[("alpha", answer("a")), ("beta", answer("b"))]).result,
            CheckResult::Pass
        );
        assert_eq!(
            caching_check(&[("alpha", answer("same")), ("beta", answer("same"))]).result,
            CheckResult::Fail,
            "spec 14.9 / acceptance 31.18"
        );
        // Nothing published is not a cache: there is no answer to go stale.
        assert_eq!(
            caching_check(&[("alpha", Vec::new()), ("beta", Vec::new())]).result,
            CheckResult::Pass
        );
        // One query cannot demonstrate anything either way, so the rule was not
        // exercised and the suite says so rather than passing vacuously.
        assert_eq!(caching_check(&[("alpha", answer("a"))]).result, CheckResult::Fail);
    }

    #[test]
    fn a_run_that_could_not_start_still_reports_every_rule() {
        let checks = unrunnable(BTreeMap::new(), "no interpreter");
        assert_eq!(checks.len(), CORE_CHECKS.len());
        for name in CORE_CHECKS {
            let check = &checks[name];
            assert_eq!(check.result, CheckResult::Fail, "{name}");
            assert!(!check.detail.is_empty(), "{name} must say why");
        }
    }

    #[test]
    fn a_windows_only_dependency_is_never_presented_as_portable() {
        let mut checks = BTreeMap::new();
        classify_portability(&mut checks, Some("keypirinha_wintypes imported by windows-only"));
        assert_eq!(
            checks["windows_only_dependencies_declared"].result,
            CheckResult::Pass,
            "detecting the dependency is a static fact and works on every host"
        );
        assert!(checks["windows_only_dependencies_declared"]
            .detail
            .contains("keypirinha_wintypes"));

        let win32 = &checks[WIN32_CHECK];
        if cfg!(windows) {
            assert_eq!(win32.result, CheckResult::Pass);
        } else {
            assert_eq!(win32.result, CheckResult::Unavailable);
            assert!(
                win32.detail.contains(host_platform()),
                "an unavailable check must name the host that could not run it: {}",
                win32.detail
            );
        }

        let mut portable = BTreeMap::new();
        classify_portability(&mut portable, None);
        assert!(
            !portable.contains_key(WIN32_CHECK),
            "a package that names no Win32 entry point has nothing to report about one"
        );
    }
}
