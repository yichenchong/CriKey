//! The two modern-Python developer commands (spec 26.3, 28; contract §7).
//!
//! * `crikey dev test --plugin DIR [--index DIR] [--query TEXT]... [--cancel]`
//!   loose-loads a modern python plugin, resolves and materialises its declared
//!   dependencies from an offline package index, spawns a real worker in a
//!   child interpreter, runs every `--query` and prints one result line per
//!   emitted item plus a run summary.
//! * `crikey dev run --plugin DIR [--index DIR] [--query TEXT]` drives a single
//!   query end to end through the same live worker: a developer smoke of the
//!   real path, not the winit GUI.
//!
//! # The output contract (identical to `dev test-legacy-compat`)
//!
//! Every line is whitespace-separated `key=value` tokens. A line carrying an
//! `item=<index>` token is one emitted result; every other line contributes to
//! the run summary. Plugin authors write item labels, so a value may hold a
//! space, an `=` or a `%`; every value is therefore percent-encoded with
//! uppercase hex ([`encode`]), which keeps `split_whitespace` then
//! `split_once('=')` a total, lossless reader.
//!
//! # Three exit statuses, not two
//!
//! A plugin that *raises* while serving a query is a completed run that found a
//! fault: it ran, learned something, and prints all of it, so it exits
//! [`EX_FOUND_FAULT`] with the report still on stdout. A `--plugin` that is not
//! a loadable modern plugin — no manifest, a missing entrypoint class, an
//! import-time raise, an absent path — is the caller's fault and exits
//! [`EX_USAGE`] with an empty stdout and a reason on stderr. `dev
//! inspect-protocol` keeps answering `EX_UNAVAILABLE` (M5); these two never do.
//!
//! # Determinism
//!
//! Nothing here samples a clock as a synchronisation primitive and no printed
//! value carries a pid, a duration or a temporary path: two runs of one
//! invocation print byte-identical stdout. Cancellation is driven by a latched
//! flag raised from another thread while the call is in flight, never by
//! elapsed time.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crikey_core::{Item, PluginId};
use crikey_package_manager::{resolve, EnvironmentInputs, EnvironmentStore, ImportPath, PackageIndex};
use crikey_plugin_model::Manifest;
use crikey_python_host::{
    discover_interpreter, sdk_root, BatchState, HostError, ModernWorker, RequiresPython, RuntimeProfile,
    SuggestRequest, Suggestions, WorkerOptions,
};

/// A completed run that found nothing wrong.
const EX_OK: u8 = 0;
/// A run that completed and found a fault (a plugin raised while serving). The
/// report is still on stdout; only the status differs.
const EX_FOUND_FAULT: u8 = 1;
/// `EX_USAGE`: the caller's fault, and the only status a bad argument list or a
/// `--plugin` that will not load may produce.
const EX_USAGE: u8 = 64;

/// Bound on the startup handshake with the child interpreter, in milliseconds.
/// A liveness guard, not a performance assertion: it turns a worker that never
/// answers into a named error instead of a hung developer command.
const STARTUP_BUDGET_MS: u64 = 30_000;
/// Bound on one modern callback. Generous: a developer smoke must not race a
/// slow plugin, and the cooperative-cancel fixture spins until cancelled.
const CALL_BUDGET_MS: u64 = 120_000;
/// Bound on an orderly shutdown.
const SHUTDOWN_BUDGET_MS: u64 = 5_000;

/// The requires-python a plugin that names none is held to. Kept in lockstep
/// with the live provider's default (`crikey-app` modern_provider) so a plugin
/// omitting `requires-python` is gated identically by `crikey dev` and by
/// `crikey run`.
const DEFAULT_REQUIRES_PYTHON: &str = ">=3.8";

/// A private temporary directory used only for one developer-command run.
///
/// The package manager reuses a committed environment by path and does not
/// re-verify its files. A predictable directory directly below the shared
/// system temporary directory would therefore let another local process plant
/// an environment before this command starts. The directory is created
/// exclusively and restricted to the current user before it is handed to the
/// package manager; dropping it removes the command's cache.
struct PrivateTempDir {
    path: PathBuf,
}

impl PrivateTempDir {
    fn new(label: &str) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        for _ in 0..256 {
            let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("crikey-dev-{label}-{pid}-{ordinal}"));
            match fs::create_dir(&path) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        let mut permissions = match fs::symlink_metadata(&path) {
                            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                                metadata.permissions()
                            }
                            Ok(_) => {
                                let _ = fs::remove_dir_all(&path);
                                return Err(format!(
                                    "temporary directory `{}` was replaced by a non-directory",
                                    path.display()
                                ));
                            }
                            Err(error) => {
                                let _ = fs::remove_dir_all(&path);
                                return Err(format!(
                                    "cannot inspect temporary directory `{}`: {error}",
                                    path.display()
                                ));
                            }
                        };
                        permissions.set_mode(0o700);
                        if let Err(error) = fs::set_permissions(&path, permissions) {
                            let _ = fs::remove_dir_all(&path);
                            return Err(format!(
                                "cannot restrict temporary directory `{}`: {error}",
                                path.display()
                            ));
                        }
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "cannot create private temporary directory `{}`: {error}",
                        path.display()
                    ))
                }
            }
        }
        Err("could not allocate a unique private temporary directory".to_owned())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub(crate) fn run(args: &[String]) -> ExitCode {
    dispatch("run", args)
}

pub(crate) fn test(args: &[String]) -> ExitCode {
    dispatch("test", args)
}

fn dispatch(command: &str, args: &[String]) -> ExitCode {
    let options = match parse_args(command, args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{}", help(command));
            return ExitCode::from(EX_OK);
        }
        Err(message) => return refuse(command, &message),
    };

    match load_and_run(command, &options) {
        Ok(report) => {
            print!("{}", report.text);
            ExitCode::from(if report.found_fault { EX_FOUND_FAULT } else { EX_OK })
        }
        // Anything that stops the run from starting — a missing manifest, a bad
        // entrypoint, a plugin that will not import — is the caller's fault and
        // prints nothing on stdout.
        Err(message) => refuse(command, &message),
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// A parsed, validated invocation. `queries` is order-preserving so a result's
/// attribution to the query that produced it stays stable across runs.
#[derive(Debug)]
struct Options {
    plugin: String,
    index: Option<String>,
    queries: Vec<String>,
    cancel: bool,
}

/// `Ok(None)` means help was asked for. Help is honoured before plugin
/// validation: a known option beside a path that cannot be loaded still
/// explains the command, while an unknown option is refused.
fn parse_args(command: &str, args: &[String]) -> Result<Option<Options>, String> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        if let Some(argument) = unknown_help_argument(command, args) {
            return Err(format!(
                "`dev {command}` does not understand `{argument}`; see `--help` for valid options"
            ));
        }
        return Ok(None);
    }
    // `--cancel` is a `dev test` affordance: `dev run` drives exactly one query
    // and has nothing to cancel, so the flag is an unknown argument there.
    let allow_cancel = command == "test";

    let mut plugin: Option<String> = None;
    let mut index: Option<String> = None;
    let mut queries: Vec<String> = Vec::new();
    let mut cancel = false;
    let mut plugin_seen = false;
    let mut index_seen = false;

    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        if let Some(value) = argument.strip_prefix("--plugin=") {
            if plugin_seen {
                return Err(format!("`dev {command}` accepts `--plugin` only once"));
            }
            plugin = Some(value.to_owned());
            plugin_seen = true;
            position += 1;
        } else if argument == "--plugin" {
            if plugin_seen {
                return Err(format!("`dev {command}` accepts `--plugin` only once"));
            }
            let value = required_value(args, position, command, "--plugin")?;
            plugin = Some(value.to_owned());
            plugin_seen = true;
            position += 2;
        } else if let Some(value) = argument.strip_prefix("--index=") {
            if index_seen {
                return Err(format!("`dev {command}` accepts `--index` only once"));
            }
            index = Some(value.to_owned());
            index_seen = true;
            position += 1;
        } else if argument == "--index" {
            if index_seen {
                return Err(format!("`dev {command}` accepts `--index` only once"));
            }
            let value = required_value(args, position, command, "--index")?;
            index = Some(value.to_owned());
            index_seen = true;
            position += 2;
        } else if let Some(value) = argument.strip_prefix("--query=") {
            queries.push(value.to_owned());
            position += 1;
        } else if argument == "--query" {
            let value = required_value(args, position, command, "--query")?;
            queries.push(value.to_owned());
            position += 2;
        } else if argument == "--cancel" && allow_cancel {
            if cancel {
                return Err("`--cancel` may only be given once".to_owned());
            }
            cancel = true;
            position += 1;
        } else {
            // Refused rather than ignored: a smoke that silently discarded half
            // its invocation reports on a plugin the caller did not describe.
            return Err(format!(
                "`dev {command}` does not understand `{argument}`; the plugin is named with \
                 `--plugin DIR`"
            ));
        }
    }
    let plugin = match plugin {
        // An empty path is refused rather than resolved: it would otherwise
        // become the process working directory and load whatever is there.
        Some(path) if path.is_empty() => {
            return Err(format!("`dev {command} --plugin` was given an empty path"))
        }
        Some(path) => path,
        None => return Err(format!("`dev {command}` needs `--plugin DIR`")),
    };

    let index = match index {
        Some(path) if path.is_empty() => {
            return Err(format!("`dev {command} --index` was given an empty path"))
        }
        other => other,
    };
    if cancel && queries.is_empty() {
        return Err("`--cancel` needs at least one `--query`".to_owned());
    }

    Ok(Some(Options {
        plugin,
        index,
        queries,
        cancel,
    }))
}

fn unknown_help_argument<'a>(command: &str, args: &'a [String]) -> Option<&'a str> {
    let allow_cancel = command == "test";
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        if matches!(argument, "-h" | "--help") {
            index += 1;
        } else if matches!(argument, "--plugin" | "--index" | "--query") {
            let Some(value) = args.get(index + 1) else {
                return Some(argument);
            };
            if value.starts_with('-') {
                return Some(value);
            }
            index += 2;
        } else if argument.starts_with("--plugin=")
            || argument.starts_with("--index=")
            || argument.starts_with("--query=")
            || (allow_cancel && argument == "--cancel")
        {
            index += 1;
        } else {
            return Some(argument);
        }
    }
    None
}

fn required_value<'a>(
    args: &'a [String],
    index: usize,
    command: &str,
    option: &str,
) -> Result<&'a str, String> {
    let value = args.get(index + 1).map(String::as_str).ok_or_else(|| {
        let noun = if option == "--query" { "text" } else { "path" };
        format!("`dev {command}` needs {noun} after `{option}`")
    })?;
    if value.starts_with('-') {
        return Err(format!(
            "`dev {command}` needs a value after `{option}`, got flag-like argument `{value}`"
        ));
    }
    Ok(value)
}

fn refuse(command: &str, message: &str) -> ExitCode {
    eprintln!("crikey: {message}\n\ncrikey dev {command} --help");
    ExitCode::from(EX_USAGE)
}

fn help(command: &str) -> String {
    let cancel = if command == "test" { " [--cancel]" } else { "" };
    format!(
        "crikey dev {command}\n\
         \n\
         USAGE:\n\
         \x20   crikey dev {command} --plugin DIR [--index DIR] [--query TEXT]...{cancel}\n\
         \x20   crikey dev {command} --help\n\
         \n\
         OPTIONS:\n\
         \x20   --plugin DIR   The modern python plugin directory to load (its `crikey.toml`).\n\
         \x20   --index DIR    An offline package index for the plugin's declared dependencies.\n\
         \x20   --query TEXT   A query to serve. Repeatable for `dev test`.\n\
         {cancel_help}\
         \x20   -h, --help     Print this message and load nothing.\n\
         \n\
         Loose-loads the modern plugin, resolves and materialises its declared dependencies,\n\
         spawns a worker in a child interpreter, and runs each query. A plugin that raises\n\
         while serving is a completed run that found a fault (exit 1 with the report on\n\
         stdout); a `--plugin` that will not load is a usage error (exit 64, nothing on\n\
         stdout).\n\
         \n\
         Output is whitespace-separated `key=value` tokens with percent-encoded values.\n",
        cancel_help = if command == "test" {
            "\x20   --cancel       Request cancellation mid-query and report whether the plugin \
             cooperated.\n"
        } else {
            ""
        }
    )
}

// ---------------------------------------------------------------------------
// Loading and running
// ---------------------------------------------------------------------------

/// The whole report plus the one bit that decides the exit status.
struct Report {
    text: String,
    found_fault: bool,
}

/// Loads the plugin, materialises its environment, spawns a worker and runs
/// every query. An `Err` means the run never started — the caller's fault.
fn load_and_run(command: &str, options: &Options) -> Result<Report, String> {
    let plugin_dir = Path::new(&options.plugin);

    let manifest_path = plugin_dir.join("crikey.toml");
    let manifest_text = fs::read_to_string(&manifest_path).map_err(|error| {
        format!(
            "`{}` is not a modern plugin: cannot read `{}`: {error}",
            options.plugin,
            manifest_path.display()
        )
    })?;
    let manifest = Manifest::parse(&manifest_text).map_err(|error| {
        format!(
            "`{}` is not a valid crikey.toml: {error}",
            manifest_path.display()
        )
    })?;

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let entrypoint = manifest
        .entrypoint_for(os, arch)
        .map_err(|error| format!("`{}` has no usable entrypoint: {error}", options.plugin))?
        .to_owned();

    let requires = manifest
        .python
        .requires_python
        .clone()
        .unwrap_or_else(|| DEFAULT_REQUIRES_PYTHON.to_owned());

    let interpreter = discover_interpreter(&RuntimeProfile::Bundled, &RequiresPython(requires.clone()))
        .map_err(|error| format!("no supported CPython for the modern worker: {error}"))?;

    // Resolve and materialise the declared dependencies. With no `--index` the
    // index is empty, which is correct for a plugin that declares no deps and a
    // resolution error for one that does — the caller's fault either way.
    let index = build_index(options.index.as_deref())?;
    let lock = resolve(&requires, &manifest.python.dependencies, &index)
        .map_err(|error| format!("dependency resolution failed: {error}"))?;
    let inputs = EnvironmentInputs {
        python_version: interpreter.version().to_string(),
        os: os.to_owned(),
        arch: arch.to_owned(),
        locked: lock.packages.clone(),
        native_build_options: Vec::new(),
    };
    let cache_root = PrivateTempDir::new("modern-cache")?;
    let store = EnvironmentStore::new(cache_root.path().to_path_buf());
    let env = store
        .ensure(&inputs, &index)
        .map_err(|error| format!("could not materialise the plugin environment: {error}"))?;

    let sdk = sdk_root();
    let import_path = ImportPath::assemble(plugin_dir, &[], &env, &sdk);
    let plugin_id = PluginId(format!("modern.{}", manifest.plugin.id));
    let worker_options = WorkerOptions::new(plugin_id, entrypoint, import_path)
        .with_startup_timeout_ms(STARTUP_BUDGET_MS)
        .with_call_timeout_ms(CALL_BUDGET_MS)
        .with_shutdown_timeout_ms(SHUTDOWN_BUDGET_MS);

    // Spawn loads the plugin's entrypoint class during the handshake, so a
    // missing class or an import-time raise fails here — an unloadable plugin,
    // not a completed run that found a fault.
    let mut worker = ModernWorker::spawn(&interpreter, worker_options).map_err(|error| {
        format!(
            "the modern plugin at `{}` could not be loaded: {error}",
            options.plugin
        )
    })?;

    // `dev run` drives exactly one query; an omitted `--query` there means the
    // empty query rather than a refusal, so the smoke always exercises the path.
    let queries: Vec<String> = if options.queries.is_empty() && command == "run" {
        vec![String::new()]
    } else {
        options.queries.clone()
    };

    let mut items_text = String::new();
    let mut item_index = 0_usize;
    let mut results = 0_usize;
    let mut any_failed = false;
    let mut any_cancelled = false;
    let mut faults: Vec<String> = Vec::new();

    for (ordinal, query) in queries.iter().enumerate() {
        let generation = u64::try_from(ordinal)
            .ok()
            .and_then(|ordinal| ordinal.checked_add(1))
            .ok_or_else(|| "too many queries for the worker generation range".to_owned())?;
        let request = SuggestRequest {
            // A distinct, monotonic generation per query, exactly as the live
            // pipeline tags keystrokes.
            generation,
            text: query.clone(),
            normalized: query.to_lowercase(),
            selected_item_id: None,
        };

        let answer = if options.cancel {
            suggest_with_cancellation(&mut worker, &request)
        } else {
            worker.suggest(&request)
        };

        match answer {
            Ok(suggestions) => {
                for item in &suggestions.items {
                    items_text.push_str(&item_line(item_index, query, item));
                    item_index += 1;
                    results += 1;
                }
                match suggestions.state {
                    BatchState::Failed => {
                        any_failed = true;
                        if let Some(error) = &suggestions.error {
                            faults.push(error.message.clone());
                        }
                    }
                    BatchState::Cancelled => any_cancelled = true,
                    BatchState::Final => {}
                }
            }
            // The transport broke (a crash, a protocol fault). The run started,
            // so this is a fault the run found, not a usage error: report it and
            // keep the exit status at "found a fault".
            Err(error) => {
                any_failed = true;
                faults.push(format!("worker error while serving `{query}`: {error}"));
            }
        }
    }

    let worker_exit = worker.shutdown();
    if !any_failed && (worker_exit.code != Some(0) || worker_exit.hard_stopped) {
        any_failed = true;
        faults.push(format!(
            "modern worker did not exit cleanly (code={:?}, hard_stopped={})",
            worker_exit.code, worker_exit.hard_stopped
        ));
    }

    let state = if any_failed {
        "failed"
    } else if any_cancelled {
        "cancelled"
    } else {
        "final"
    };

    let mut text = String::new();
    field(&mut text, "plugin", &manifest.plugin.id);
    field(&mut text, "queries", &queries.len().to_string());
    field(&mut text, "results", &results.to_string());
    field(&mut text, "state", state);
    if options.cancel {
        // A plugin cooperated with cancellation exactly when it observed the
        // request and returned a `cancelled` batch rather than running on.
        field(
            &mut text,
            "cooperated",
            if any_cancelled { "true" } else { "false" },
        );
    }
    if !faults.is_empty() {
        // One field, so two failed queries cannot collide on a repeated key;
        // the messages are joined losslessly and percent-encoded like the rest.
        field(&mut text, "fault", &faults.join("; "));
    }
    text.push_str(&items_text);

    Ok(Report {
        text,
        found_fault: any_failed,
    })
}

/// Loads the offline package index, or an empty one when `--index` is absent.
///
/// An empty index is not an error: a plugin that declares no dependencies needs
/// none, and one that declares a dependency the index cannot satisfy is a
/// resolution failure the caller can fix.
fn build_index(index: Option<&str>) -> Result<PackageIndex, String> {
    match index {
        Some(dir) if !dir.is_empty() => PackageIndex::from_dir(Path::new(dir))
            .map_err(|error| format!("cannot read package index `{dir}`: {error}")),
        _ => {
            let empty = PrivateTempDir::new("modern-empty-index")?;
            PackageIndex::from_dir(empty.path())
                .map_err(|error| format!("cannot load the empty package index: {error}"))
        }
    }
}

/// Serves one query with a cancellation raised before the call is submitted.
///
/// The child latches the cancel flag until an explicit reset (never at
/// suggest-start), and its control-reader is live from the handshake, so
/// raising the flag first and then submitting the query is race-free: a plugin
/// that polls `context.cancelled` sees it and returns a `cancelled` batch, with
/// no second thread and no interleaved writes to the child's stdin.
fn suggest_with_cancellation(
    worker: &mut ModernWorker,
    request: &SuggestRequest,
) -> Result<Suggestions, HostError> {
    worker.cancel_handle().cancel();
    worker.suggest_with_cancel_latched(request)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// One emitted result, attributed to the query that produced it.
fn item_line(index: usize, query: &str, item: &Item) -> String {
    let mut line = String::new();
    for (key, value) in [
        ("item", index.to_string()),
        ("query", query.to_owned()),
        ("stable_id", item.stable_id.0.clone()),
        ("label", item.label.clone()),
        ("target", item.target.clone()),
    ] {
        if !line.is_empty() {
            line.push(' ');
        }
        write!(line, "{key}={}", encode(&value)).expect("writing to a String cannot fail");
    }
    line.push('\n');
    line
}

/// One summary line, its value percent-encoded like every other.
fn field(out: &mut String, key: &str, value: &str) {
    writeln!(out, "{key}={}", encode(value)).expect("writing to a String cannot fail");
}

/// Percent-encodes a value so a line always splits into `key=value` tokens.
///
/// Token-safe ASCII (unreserved characters plus `/` and `:`) survives
/// unescaped. Everything else — the space that would split a token, the `=`
/// that would split a pair, the `%` that would make an escape ambiguous, and
/// every non-ASCII byte — becomes `%XX` with uppercase hex. One spelling per
/// escape, so two runs never diff on case.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn developer_temp_directories_are_private_and_unique() {
        let first = PrivateTempDir::new("test").expect("first private directory");
        let second = PrivateTempDir::new("test").expect("second private directory");
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        assert!(second.path().is_dir());
        #[cfg(unix)]
        {
            let first_mode = fs::metadata(first.path())
                .expect("first directory metadata")
                .permissions()
                .mode()
                & 0o777;
            let second_mode = fs::metadata(second.path())
                .expect("second directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(first_mode, 0o700);
            assert_eq!(second_mode, 0o700);
        }
    }

    #[test]
    fn help_does_not_hide_unknown_modern_options_or_empty_indexes() {
        let help = vec!["--help".to_owned(), "--unknown".to_owned()];
        assert!(parse_args("test", &help).is_err());
        let empty_index = vec!["--plugin".to_owned(), "plugin".to_owned(), "--index=".to_owned()];
        assert!(parse_args("test", &empty_index).is_err());
    }
    #[test]
    fn separate_option_values_cannot_consume_the_next_flag() {
        let query_value = vec![
            "--plugin".to_owned(),
            "plugin".to_owned(),
            "--query".to_owned(),
            "--unknown".to_owned(),
        ];
        assert!(parse_args("test", &query_value).is_err());

        let plugin_value = vec!["--plugin".to_owned(), "--index".to_owned(), "--help".to_owned()];
        assert!(parse_args("test", &plugin_value).is_err());
        assert!(parse_args("test", &["--help".to_owned(), "--query".to_owned()]).is_err());
    }

    #[test]
    fn scalar_options_cannot_be_silently_replaced() {
        let duplicate_plugin = vec![
            "--plugin".to_owned(),
            "first".to_owned(),
            "--plugin=second".to_owned(),
        ];
        assert!(parse_args("test", &duplicate_plugin).is_err());

        let duplicate_index = vec![
            "--plugin".to_owned(),
            "plugin".to_owned(),
            "--index".to_owned(),
            "first".to_owned(),
            "--index=second".to_owned(),
        ];
        assert!(parse_args("test", &duplicate_index).is_err());

        let duplicate_cancel = vec![
            "--plugin".to_owned(),
            "plugin".to_owned(),
            "--cancel".to_owned(),
            "--cancel".to_owned(),
        ];
        assert!(parse_args("test", &duplicate_cancel).is_err());
    }

    #[test]
    fn cancellation_requires_a_query_to_cancel() {
        let args = vec!["--plugin".to_owned(), "plugin".to_owned(), "--cancel".to_owned()];
        assert!(parse_args("test", &args).is_err());
    }
}
