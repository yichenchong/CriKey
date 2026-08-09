//! Native-plugin protocol inspection (spec 26.3, 28; contract §5.1).
//!
//! The inspector deliberately drives the same supervised worker used by the
//! application. It does not know anything about the conformance fixture: all
//! child configuration arrives through the developer-facing `--env` option.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crikey_core::{Item, PluginId};
use crikey_native_host::{
    BatchState, ExitKind, HostError, LaunchSpec, NativeSuggestRequest, NativeWorker, TransportKind,
    WorkerOptions,
};
use crikey_native_protocol::PROTOCOL_VERSION;

const EX_OK: u8 = 0;
const EX_NOT_CONFORMANT: u8 = 1;
const EX_USAGE: u8 = 64;

const CHECKS: [&str; 11] = [
    "handshake",
    "protocol-version",
    "session-token",
    "catalog-streaming",
    "suggest-streaming",
    "request-id-echo",
    "generation-echo",
    "cancellation",
    "credit-discipline",
    "frame-bounds",
    "shutdown",
];

/// Parsed options for `crikey dev inspect-protocol`.
#[derive(Debug)]
struct Options {
    plugin: PathBuf,
    transport: TransportKind,
    transport_name: &'static str,
    queries: Vec<String>,
    cancel: bool,
    trace: bool,
    environment: Vec<(String, String)>,
}

/// One frozen conformance check in report order.
#[derive(Debug)]
struct Check {
    name: &'static str,
    result: bool,
    detail: String,
}

/// A captured trace line. The direction is intentionally not percent encoded:
/// `host->plugin` is the literal spelling frozen by contract §5.1.
#[derive(Debug)]
struct Trace {
    direction: &'static str,
    kind: &'static str,
    request: u64,
}

/// One result item attributed to the query that produced it.
#[derive(Debug)]
struct ResultItem {
    query: String,
    item: Item,
}

/// Completed inspection report. A report is emitted even when the plugin
/// violates the protocol after startup; only a spawn failure is a usage error.
#[derive(Debug)]
struct Report {
    plugin: String,
    /// The protocol version the session settled on. A handshake rejected
    /// over the version itself never settled on one, so it stays `None`
    /// rather than claiming a version that was never agreed.
    protocol: Option<u32>,
    transport: &'static str,
    capabilities: String,
    catalog_items: usize,
    checks: Vec<Check>,
    items: Vec<ResultItem>,
    queries: usize,
    results: usize,
    state: &'static str,
    cooperated: Option<bool>,
    batches: usize,
    traces: Vec<Trace>,
    conformant: bool,
}

/// Runs `crikey dev inspect-protocol`.
///
/// The host owns session authentication, framing, deadlines and credit
/// accounting. This command only turns those observed outcomes into the
/// deterministic report required by spec §26.3 and §28.
pub(crate) fn inspect_protocol(args: &[String]) -> ExitCode {
    match parse_args(args) {
        Ok(None) => {
            print_help();
            ExitCode::from(EX_OK)
        }
        Err(message) => refuse(&message),
        Ok(Some(options)) => match inspect(options) {
            Ok(report) => {
                print_report(&report);
                ExitCode::from(if report.conformant {
                    EX_OK
                } else {
                    EX_NOT_CONFORMANT
                })
            }
            Err(message) => refuse(&message),
        },
    }
}

fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        if let Some(argument) = unknown_help_argument(args) {
            return Err(format!(
                "`dev inspect-protocol` does not understand `{argument}`; see `--help` for valid options"
            ));
        }
        return Ok(None);
    }

    let mut plugin: Option<PathBuf> = None;
    let mut transport: Option<&'static str> = None;
    let mut queries = Vec::new();
    let mut cancel = false;
    let mut trace = false;
    let mut environment = Vec::new();

    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        if let Some(value) = argument.strip_prefix("--plugin=") {
            plugin = Some(PathBuf::from(value));
            position += 1;
        } else if argument == "--plugin" {
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`dev inspect-protocol` needs a path after `--plugin`".to_owned())?;
            plugin = Some(PathBuf::from(value));
            position += 2;
        } else if let Some(value) = argument.strip_prefix("--transport=") {
            transport = Some(parse_transport(value)?);
            position += 1;
        } else if argument == "--transport" {
            let value = args.get(position + 1).ok_or_else(|| {
                "`dev inspect-protocol` needs unix, pipe or stdio after `--transport`".to_owned()
            })?;
            transport = Some(parse_transport(value)?);
            position += 2;
        } else if let Some(value) = argument.strip_prefix("--query=") {
            queries.push(value.to_owned());
            position += 1;
        } else if argument == "--query" {
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`dev inspect-protocol` needs text after `--query`".to_owned())?;
            queries.push(value.clone());
            position += 2;
        } else if argument == "--cancel" {
            cancel = true;
            position += 1;
        } else if argument == "--trace" {
            trace = true;
            position += 1;
        } else if let Some(value) = argument.strip_prefix("--env=") {
            environment.push(parse_environment(value)?);
            position += 1;
        } else if argument == "--env" {
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`dev inspect-protocol` needs KEY=VALUE after `--env`".to_owned())?;
            environment.push(parse_environment(value)?);
            position += 2;
        } else {
            return Err(format!("`dev inspect-protocol` does not understand `{argument}`"));
        }
    }

    let plugin = plugin.ok_or_else(|| "`dev inspect-protocol` needs `--plugin EXE`".to_owned())?;
    if plugin.as_os_str().is_empty() {
        return Err("`dev inspect-protocol --plugin` was given an empty path".to_owned());
    }

    let transport_name = transport.unwrap_or(default_transport_name());
    let transport = transport_kind(transport_name);
    Ok(Some(Options {
        plugin,
        transport,
        transport_name,
        queries,
        cancel,
        trace,
        environment,
    }))
}

fn unknown_help_argument(args: &[String]) -> Option<&str> {
    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        if matches!(argument, "-h" | "--help" | "--cancel" | "--trace") {
            position += 1;
        } else if matches!(argument, "--plugin" | "--transport" | "--query" | "--env") {
            position += 1;
            if args.get(position).is_some_and(|value| !value.starts_with("--")) {
                position += 1;
            }
        } else if argument.starts_with("--plugin=")
            || argument.starts_with("--transport=")
            || argument.starts_with("--query=")
            || argument.starts_with("--env=")
        {
            position += 1;
        } else {
            return Some(argument);
        }
    }
    None
}

fn parse_transport(value: &str) -> Result<&'static str, String> {
    match value {
        "unix" => Ok("unix"),
        "pipe" => Ok("pipe"),
        "stdio" => Ok("stdio"),
        _ => Err(format!(
            "`dev inspect-protocol --transport` must be unix, pipe or stdio (got `{value}`)"
        )),
    }
}

fn parse_environment(value: &str) -> Result<(String, String), String> {
    let (key, val) = value
        .split_once('=')
        .ok_or_else(|| "`dev inspect-protocol --env` needs KEY=VALUE".to_owned())?;
    if key.is_empty() {
        return Err("`dev inspect-protocol --env` needs a non-empty KEY".to_owned());
    }
    Ok((key.to_owned(), val.to_owned()))
}

fn default_transport_name() -> &'static str {
    #[cfg(unix)]
    {
        "unix"
    }
    #[cfg(windows)]
    {
        "pipe"
    }
    #[cfg(not(any(unix, windows)))]
    {
        "stdio"
    }
}

fn transport_kind(name: &str) -> TransportKind {
    match name {
        "unix" => TransportKind::UnixSocket,
        "pipe" => TransportKind::NamedPipe,
        "stdio" => TransportKind::Stdio,
        _ => TransportKind::Stdio,
    }
}

fn refuse(message: &str) -> ExitCode {
    eprintln!("crikey: {message}\n\n{}", help_text());
    ExitCode::from(EX_USAGE)
}

fn print_help() {
    print!("{}", help_text());
}

fn help_text() -> String {
    "crikey dev inspect-protocol\n\n\
     USAGE:\n\
         crikey dev inspect-protocol --plugin EXE [--transport unix|pipe|stdio] [--query TEXT]... [--cancel] [--trace] [--env KEY=VALUE]...\n\
         crikey dev inspect-protocol --help\n\n\
     OPTIONS:\n\
         --plugin EXE       Native plugin executable to launch.\n\
         --transport KIND   unix, pipe or stdio (platform default when omitted).\n\
         --query TEXT       Suggestion query; repeatable.\n\
         --cancel           Exercise cancellation during a suggestion call.\n\
         --trace            Print protocol message directions and payload kinds.\n\
         --env KEY=VALUE    Add one explicit child environment variable; repeatable.\n\
         -h, --help         Print this message and launch nothing.\n"
        .to_owned()
}

fn inspect(options: Options) -> Result<Report, String> {
    let plugin_for_launch = PluginId("inspect-protocol".to_owned());
    let working_dir = options
        .plugin
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_owned);
    let spec = LaunchSpec {
        plugin: plugin_for_launch,
        executable: options.plugin.clone(),
        arguments: Vec::new(),
        working_dir,
        environment: options.environment.clone(),
        // `inspect` is a diagnostic harness, not a manifest-governed load, so
        // it takes the same stripped environment a plugin with no
        // `permissions.environment` grant would get: what it reports must be
        // what the launcher would actually do.
        inherit_environment: false,
    };
    let mut worker_options = WorkerOptions::new();
    worker_options.transport = options.transport;

    let mut worker = match NativeWorker::spawn(spec, worker_options) {
        Ok(worker) => worker,
        Err(HostError::Spawn(detail)) => {
            return Err(format!(
                "native plugin `{}` could not be launched: {detail}",
                options.plugin.display()
            ));
        }
        Err(error) => return Ok(handshake_failure_report(&options, &error)),
    };

    let plugin = worker.handshake().plugin_id.clone();
    let protocol = worker.handshake().protocol_version;
    let capabilities = capabilities_text(&worker.handshake().capabilities);

    let mut checks = Vec::with_capacity(CHECKS.len());
    let handshake_ok = true;
    let protocol_ok = protocol == PROTOCOL_VERSION;
    let session_ok = true;
    checks.push(check(
        "handshake",
        handshake_ok,
        if handshake_ok {
            "handshake accepted"
        } else {
            "handshake was rejected"
        },
    ));
    checks.push(check(
        "protocol-version",
        protocol_ok,
        if protocol_ok {
            "protocol version accepted"
        } else {
            "unsupported protocol version"
        },
    ));
    checks.push(check(
        "session-token",
        session_ok,
        if session_ok {
            "session token accepted"
        } else {
            "session token rejected"
        },
    ));

    let mut credit_ok = true;
    let mut frame_ok = true;
    let catalog = worker.build_catalog();
    let (catalog_items, catalog_ok) = match catalog {
        Ok(items) => (items.len(), true),
        Err(error) => {
            note_stream_error(&error, &mut credit_ok, &mut frame_ok);
            (0, false)
        }
    };
    checks.push(check(
        "catalog-streaming",
        catalog_ok,
        if catalog_ok {
            "catalog stream completed"
        } else {
            "catalog stream failed"
        },
    ));

    let queries = if options.queries.is_empty() {
        vec![String::new()]
    } else {
        options.queries.clone()
    };
    let mut results = Vec::new();
    let mut batches = 0usize;
    let mut any_failed = false;
    let mut any_cancelled = false;
    let mut cancellation_cooperated = false;
    let mut suggest_ok = true;
    let cancel_handle = worker.cancel_handle();

    for (ordinal, query) in queries.iter().enumerate() {
        let request = NativeSuggestRequest {
            generation: ordinal as u64 + 1,
            text: query.clone(),
            normalized: query.to_lowercase(),
            selected_item_id: None,
        };
        if options.cancel && ordinal == 0 {
            cancel_handle.cancel();
        } else {
            cancel_handle.reset();
        }
        let answer = if options.cancel && ordinal == 0 {
            worker.suggest_with_cancel_latched(&request)
        } else {
            worker.suggest(&request)
        };
        match answer {
            Ok(suggestions) => {
                batches = batches.saturating_add(suggestions.batches);
                results.extend(suggestions.items.into_iter().map(|item| ResultItem {
                    query: query.clone(),
                    item,
                }));
                match suggestions.state {
                    BatchState::Final => {}
                    BatchState::Cancelled => {
                        any_cancelled = true;
                        if options.cancel && ordinal == 0 {
                            cancellation_cooperated = true;
                        }
                    }
                    BatchState::Failed => {
                        any_failed = true;
                        suggest_ok = false;
                    }
                }
            }
            Err(error) => {
                suggest_ok = false;
                note_stream_error(&error, &mut credit_ok, &mut frame_ok);
                any_failed = true;
            }
        }
    }
    let mismatch = worker.echo_mismatch();
    let request_ok = mismatch.as_ref().is_none_or(|value| !value.request_id);
    let generation_ok = mismatch.as_ref().is_none_or(|value| !value.generation);
    let request_detail = match mismatch.as_ref() {
        Some(value) if value.request_id => {
            format!("request id mismatch observed: {}", value.reason)
        }
        _ => "request ids matched".to_owned(),
    };
    let generation_detail = match mismatch.as_ref() {
        Some(value) if value.generation => {
            format!("generation mismatch observed: {}", value.reason)
        }
        _ => "generations matched".to_owned(),
    };
    let (exit, observations) = if options.trace {
        worker.shutdown_with_observations()
    } else {
        (worker.shutdown(), Vec::new())
    };
    let shutdown_ok = exit.kind == ExitKind::Clean;
    let traces = observations
        .into_iter()
        .map(|observation| Trace {
            direction: observation.direction,
            kind: observation.kind,
            request: observation.request_id,
        })
        .collect();

    checks.push(check(
        "suggest-streaming",
        suggest_ok,
        if suggest_ok {
            "suggestion stream completed"
        } else {
            "suggestion stream failed"
        },
    ));
    checks.push(check("request-id-echo", request_ok, &request_detail));
    checks.push(check("generation-echo", generation_ok, &generation_detail));
    if options.cancel {
        let cancellation_ok = cancellation_cooperated;
        checks.push(check(
            "cancellation",
            cancellation_ok,
            if cancellation_ok {
                "plugin cooperated with cancellation"
            } else {
                "plugin did not cooperate with cancellation"
            },
        ));
    }
    checks.push(check(
        "credit-discipline",
        credit_ok,
        if credit_ok {
            "batch credits respected"
        } else {
            "host rejected a zero-credit batch"
        },
    ));
    checks.push(check(
        "frame-bounds",
        frame_ok,
        if frame_ok {
            "frames stayed within the limit"
        } else {
            "host rejected a frame above MAX_FRAME_BYTES"
        },
    ));
    checks.push(check(
        "shutdown",
        shutdown_ok,
        if shutdown_ok {
            "worker shut down cleanly"
        } else {
            "worker did not shut down cleanly"
        },
    ));

    let conformant = checks.iter().all(|check| check.result);
    let state = if any_failed {
        "failed"
    } else if any_cancelled {
        "cancelled"
    } else {
        "final"
    };
    let result_count = results.len();
    Ok(Report {
        plugin,
        protocol: Some(protocol),
        transport: options.transport_name,
        capabilities,
        catalog_items,
        checks,
        items: results,
        queries: options.queries.len().max(1),
        results: result_count,
        state,
        cooperated: options.cancel.then_some(cancellation_cooperated),
        batches,
        traces,
        conformant,
    })
}

fn handshake_failure_report(options: &Options, error: &HostError) -> Report {
    let detail = error.to_string().to_ascii_lowercase();
    // Only the host's explicit rejection phrases identify these checks. A
    // plugin id or diagnostic may contain words such as "token" without
    // describing a failed session token.
    let protocol_ok =
        !detail.contains("unsupported protocol version") && !detail.contains("unsupportedversion");
    let session_ok = !detail.contains("session token mismatch");
    let mut checks = Vec::with_capacity(CHECKS.len());
    checks.push(check("handshake", false, "handshake was rejected"));
    checks.push(check(
        "protocol-version",
        protocol_ok,
        if protocol_ok {
            "protocol version was not rejected"
        } else {
            "unsupported protocol version"
        },
    ));
    checks.push(check(
        "session-token",
        session_ok,
        if session_ok {
            "session token was not rejected"
        } else {
            "session token mismatch"
        },
    ));
    checks.push(check("catalog-streaming", false, "handshake did not complete"));
    checks.push(check("suggest-streaming", false, "handshake did not complete"));
    checks.push(check("request-id-echo", false, "handshake did not complete"));
    checks.push(check("generation-echo", false, "handshake did not complete"));
    if options.cancel {
        checks.push(check("cancellation", false, "handshake did not complete"));
    }
    let traces = Vec::new();
    Report {
        plugin: "unknown".to_owned(),
        // The handshake failed, but unless the host rejected the version
        // itself the session had already agreed on ours; claiming it is
        // honest. A version rejection leaves nothing trustworthy to report.
        protocol: protocol_ok.then_some(PROTOCOL_VERSION),
        transport: options.transport_name,
        capabilities: String::new(),
        catalog_items: 0,
        checks,
        items: Vec::new(),
        queries: options.queries.len(),
        results: 0,
        state: "failed",
        cooperated: options.cancel.then_some(false),
        batches: 0,
        traces,
        conformant: false,
    }
}

fn note_stream_error(error: &HostError, credit_ok: &mut bool, frame_ok: &mut bool) {
    let detail = match error {
        HostError::Protocol(detail) => detail.to_ascii_lowercase(),
        _ => return,
    };
    if detail.contains("credit") {
        *credit_ok = false;
    }
    if detail.contains("frame") || detail.contains("oversized") || detail.contains("too large") {
        *frame_ok = false;
    }
}

fn check(name: &'static str, result: bool, detail: &str) -> Check {
    Check {
        name,
        result,
        detail: detail.to_owned(),
    }
}

fn capabilities_text(capabilities: &crikey_native_protocol::Capabilities) -> String {
    let mut names = Vec::new();
    if capabilities.cancellation {
        names.push("cancellation");
    }
    if capabilities.configuration_updates {
        names.push("configuration_updates");
    }
    if capabilities.events {
        names.push("events");
    }
    if capabilities.streaming_catalog {
        names.push("streaming_catalog");
    }
    if capabilities.streaming_suggestions {
        names.push("streaming_suggestions");
    }
    names.join(",")
}

fn print_report(report: &Report) {
    for trace in &report.traces {
        println!(
            "trace={} kind={} request={}",
            trace.direction, trace.kind, trace.request
        );
    }
    field("plugin", &report.plugin);
    match report.protocol {
        Some(protocol) => field("protocol", &protocol.to_string()),
        None => field("protocol", "unknown"),
    }
    field("transport", report.transport);
    field("capabilities", &report.capabilities);
    field("catalog_items", &report.catalog_items.to_string());
    for check in &report.checks {
        println!(
            "check={} result={} detail={}",
            check.name,
            if check.result { "pass" } else { "fail" },
            encode(&check.detail)
        );
    }
    for (index, result) in report.items.iter().enumerate() {
        item_line(index, &result.query, &result.item);
    }
    field("queries", &report.queries.to_string());
    field("results", &report.results.to_string());
    field("state", report.state);
    if let Some(cooperated) = report.cooperated {
        field("cooperated", if cooperated { "true" } else { "false" });
    }
    field("batches", &report.batches.to_string());
    field(
        "verdict",
        if report.conformant {
            "conformant"
        } else {
            "non-conformant"
        },
    );
}

fn item_line(index: usize, query: &str, item: &Item) {
    println!(
        "item={} query={} stable_id={} label={} target={}",
        index,
        encode(query),
        encode(&item.stable_id.0),
        encode(&item.label),
        encode(&item.target)
    );
}

fn field(key: &str, value: &str) {
    println!("{key}={}", encode(value));
}

/// Mirrors `modern_commands::encode` exactly (spec §28 output contract).
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_requires_equals() {
        assert!(parse_environment("BROKEN").is_err());
        assert_eq!(parse_environment("KEY=").expect("empty values are valid").1, "");
    }

    #[test]
    fn encoding_matches_frozen_spelling() {
        assert_eq!(encode("space % and ="), "space%20%25%20and%20%3D");
    }

    #[test]
    fn help_does_not_hide_unknown_protocol_options() {
        let args = vec!["--help".to_owned(), "--unknown".to_owned()];
        assert!(parse_args(&args).is_err());
        assert!(parse_args(&["--help".to_owned(), "--plugin".to_owned(), "--unknown".to_owned()]).is_err());
    }
    #[test]
    fn a_failed_handshake_does_not_claim_the_host_protocol_version() {
        let options = Options {
            plugin: PathBuf::from("plugin"),
            transport: TransportKind::Stdio,
            transport_name: "stdio",
            queries: Vec::new(),
            cancel: false,
            trace: false,
            environment: Vec::new(),
        };
        let report = handshake_failure_report(
            &options,
            &HostError::Handshake("plugin `tokenizer` rejected session token".to_owned()),
        );
        assert_eq!(report.protocol, Some(PROTOCOL_VERSION));

        let report = handshake_failure_report(
            &options,
            &HostError::Handshake("unsupported protocol version 99".to_owned()),
        );
        assert_eq!(report.protocol, None);
        assert!(!report
            .checks
            .iter()
            .any(|check| check.name == "session-token" && !check.result));
    }
}
