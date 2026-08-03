//! Third-party-shaped native plugin used by the M5 conformance acceptance test.
//!
//! This binary intentionally depends only on the published Rust SDK and core
//! model. It is an out-of-tree process: the host launches it and communicates
//! over the endpoint named by the host-set environment (spec 16.1, 16.6,
//! acceptance §31.21, §31.22).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crikey_core::{CoreError, Result};
use crikey_plugin_sdk::{
    serve, CatalogSink, ExecuteRequest, ItemBuilder, Plugin, PluginContext, Query, SdkError, ServeConfig,
    SuggestionSink,
};

const MODE_ENV: &str = "CRIKEY_CONFORMANCE_MODE";
const MODE_FILE: &str = "conformance-mode";
const PLUGIN_ID: &str = "conformance";
const PLUGIN_NAME: &str = "CriKey Native Conformance";
const PLUGIN_VERSION: &str = "1.0.0";
const SHARED_PLUGIN_ID: &str = "shared.identity";

#[derive(Debug, Clone)]
enum Mode {
    Echo,
    SameId,
    EnvWitness,
    Stream(usize),
    Acceptance,
    Slow(u64),
    SlowWitness(u64),
    IgnoreCancel(u64),
    CrashOnSuggest,
    CrashOnStart,
    FailSuggest,
    Sequence,
}

fn candidate(value: Option<String>) -> Option<String> {
    value
        .map(|mode| mode.trim().to_owned())
        .filter(|mode| !mode.is_empty())
}

fn selected_mode() -> String {
    let from_file = || {
        let path: PathBuf = env::current_dir().ok()?.join(MODE_FILE);
        fs::read_to_string(path).ok()
    };

    candidate(env::var(MODE_ENV).ok())
        .or_else(|| candidate(env::args().nth(1)))
        .or_else(|| candidate(from_file()))
        .unwrap_or_else(|| "echo".to_owned())
}
fn parse_mode(spec: &str) -> Mode {
    match spec {
        "echo" => Mode::Echo,
        "same-id" => Mode::SameId,
        "env-witness" => Mode::EnvWitness,
        "acceptance" => Mode::Acceptance,
        "crash-on-suggest" => Mode::CrashOnSuggest,
        "crash-on-start" => Mode::CrashOnStart,
        "fail-suggest" => Mode::FailSuggest,
        "sequence" => Mode::Sequence,
        _ if spec.starts_with("stream:") => Mode::Stream(
            spec.strip_prefix("stream:")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0),
        ),
        _ if spec.starts_with("slow-witness:") => Mode::SlowWitness(
            spec.strip_prefix("slow-witness:")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        ),
        _ if spec.starts_with("slow:") => Mode::Slow(
            spec.strip_prefix("slow:")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        ),
        _ if spec.starts_with("ignore-cancel:") => Mode::IgnoreCancel(
            spec.strip_prefix("ignore-cancel:")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        ),
        _ => Mode::Echo,
    }
}

fn sequence_stage() -> Mode {
    let path = env::var_os("CRIKEY_SEQUENCE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join(format!("crikey-sequence-{}", std::process::id())));
    let count = fs::read_to_string(&path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let _ = fs::write(&path, count.saturating_add(1).to_string());
    match count {
        0 | 2 => Mode::CrashOnSuggest,
        _ => Mode::Echo,
    }
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::Invalid(message.into())
}

fn pid_target() -> String {
    std::process::id().to_string()
}

fn item(
    stable_id: impl Into<String>,
    label: impl Into<String>,
    target: impl Into<String>,
) -> crikey_core::Item {
    ItemBuilder::new(stable_id, label).target(target).build()
}

fn catalog_items() -> Vec<crikey_core::Item> {
    vec![
        pid_item(),
        item("catalog-1", "Conformance one", "conformance://one"),
        item("catalog-2", "Conformance two", "conformance://two"),
    ]
}

fn catalog_stream_items(count: usize) -> Vec<crikey_core::Item> {
    (0..count)
        .map(|index| {
            item(
                format!("stream-catalog-{index}"),
                format!("Conformance catalog #{index}"),
                format!("conformance://catalog-{index}"),
            )
        })
        .collect()
}

fn environment_items() -> Vec<crikey_core::Item> {
    let mut variables: Vec<(String, String)> = env::vars().collect();
    variables.sort_by(|left, right| left.0.cmp(&right.0));
    variables
        .into_iter()
        .map(|(name, value)| item(format!("env:{name}"), name.clone(), value))
        .collect()
}

/// Suggestion results carry a DETERMINISTIC target: `crikey dev
/// inspect-protocol` prints item targets and its output must be byte-for-byte
/// reproducible across runs, so a volatile value like the process id can never
/// live here. The pid is reported by `pid_item` instead, which only the
/// out-of-process proof (§31.30) asks for.
fn result_item(stable_id: impl Into<String>, label: impl Into<String>) -> crikey_core::Item {
    let stable_id = stable_id.into();
    let target = format!("conformance://{stable_id}");
    item(stable_id, label, target)
}

/// The one item whose target is deliberately volatile: this process's id, so a
/// caller can prove the plugin ran outside the host process (§31.30).
fn pid_item() -> crikey_core::Item {
    item("pid", "conformance process", pid_target())
}

fn wait_for_cancellation(context: &dyn PluginContext, milliseconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(milliseconds);
    loop {
        if context.cancellation().is_cancelled() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn emit_final(sink: &mut dyn SuggestionSink, items: Vec<crikey_core::Item>) -> Result<()> {
    if !items.is_empty() {
        sink.emit_batch(items)?;
    }
    sink.finish()
}

#[derive(Debug)]
struct ConformancePlugin {
    mode: Mode,
}

impl ConformancePlugin {
    fn new(mode: Mode) -> Self {
        Self { mode }
    }

    fn suggest_echo(&mut self, query: Query, sink: &mut dyn SuggestionSink) -> Result<()> {
        let first = result_item("echo-1", query.text.clone());
        let second = result_item("echo-2", format!("{} (second)", query.text));
        sink.emit_batch(vec![first])?;
        sink.emit_batch(vec![second])?;
        sink.finish()
    }

    fn suggest_stream(
        &mut self,
        query: Query,
        count: usize,
        pid_first: bool,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        let mut start = 0usize;
        while start < count {
            if sink.is_cancelled() {
                return sink.finish();
            }
            let end = start.saturating_add(16).min(count);
            let batch = (start..end)
                .map(|index| {
                    if pid_first && index == 0 {
                        pid_item()
                    } else {
                        result_item(format!("stream-{index}"), format!("{} #{index}", query.text))
                    }
                })
                .collect();
            sink.emit_batch(batch)?;
            start = end;
        }
        sink.finish()
    }

    fn suggest_acceptance(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        if query.text.starts_with("slow") {
            self.suggest_slow(query, 2_000, context, sink)
        } else {
            self.suggest_stream(query, 35, true, sink)
        }
    }

    fn suggest_slow(
        &mut self,
        query: Query,
        milliseconds: u64,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        if wait_for_cancellation(context, milliseconds) {
            return sink.finish();
        }
        emit_final(
            sink,
            vec![result_item("slow-1", format!("{} (slow)", query.text))],
        )
    }
    fn suggest_slow_witness(
        &mut self,
        query: Query,
        milliseconds: u64,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        let initial = sink.emit_batch(vec![result_item("slow-start", query.text.clone())]);
        if let Err(error) = initial {
            if context.cancellation().is_cancelled() || sink.is_cancelled() {
                return sink.finish();
            }
            return Err(error);
        }
        if wait_for_cancellation(context, milliseconds) {
            return sink.finish();
        }
        sink.finish()
    }
    fn suggest_ignore_cancel(
        &mut self,
        query: Query,
        milliseconds: u64,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        thread::sleep(Duration::from_millis(milliseconds));
        emit_final(
            sink,
            vec![result_item(
                "ignore-cancel-1",
                format!("{} (ignored)", query.text),
            )],
        )
    }
}

/// Dies the way a real native plugin dies: a diagnostic on stderr, then the
/// abort. The host's contract is that it CAPTURES the child's crash output
/// (§16.6, §26.1), so a fixture that aborts silently would let a host which
/// captures nothing still look correct.
fn crash(reason: &str) -> ! {
    use std::io::Write;
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "[crikey-conformance] fatal: {reason}");
    let _ = stderr.flush();
    std::process::abort();
}

impl Plugin for ConformancePlugin {
    fn start(&mut self, _context: &dyn PluginContext) -> Result<()> {
        if matches!(self.mode, Mode::CrashOnStart) {
            crash("crash-on-start requested");
        }
        Ok(())
    }

    fn build_catalog(&mut self, _context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        if let Mode::Stream(count) = self.mode.clone() {
            let mut batch = Vec::with_capacity(16);
            for catalog_item in catalog_stream_items(count) {
                batch.push(catalog_item);
                if batch.len() == 16 {
                    sink.emit_batch(std::mem::take(&mut batch))?;
                }
            }
            if !batch.is_empty() {
                sink.emit_batch(batch)?;
            }
            return sink.finish();
        }

        let items = if matches!(self.mode, Mode::EnvWitness) {
            environment_items()
        } else {
            catalog_items()
        };
        sink.emit_batch(items)?;
        sink.finish()
    }

    fn suggest(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        match self.mode.clone() {
            Mode::Echo | Mode::SameId => self.suggest_echo(query, sink),
            Mode::EnvWitness => emit_final(sink, environment_items()),
            Mode::Acceptance => self.suggest_acceptance(query, context, sink),
            Mode::Stream(count) => self.suggest_stream(query, count, false, sink),
            Mode::Slow(milliseconds) => self.suggest_slow(query, milliseconds, context, sink),
            Mode::SlowWitness(milliseconds) => self.suggest_slow_witness(query, milliseconds, context, sink),
            Mode::IgnoreCancel(milliseconds) => self.suggest_ignore_cancel(query, milliseconds, sink),
            Mode::CrashOnSuggest => crash("crash-on-suggest requested"),
            Mode::CrashOnStart => Err(invalid("crash-on-start should not receive suggest")),
            Mode::FailSuggest => Err(invalid("conformance fail-suggest requested")),
            Mode::Sequence => Err(invalid("sequence mode must be resolved before serving")),
        }
    }

    fn execute(&mut self, _request: ExecuteRequest, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
}

fn run() -> Result<(), SdkError> {
    let mode = match parse_mode(&selected_mode()) {
        Mode::Sequence => sequence_stage(),
        mode => mode,
    };
    let configured_id = if matches!(mode, Mode::SameId) {
        SHARED_PLUGIN_ID
    } else {
        PLUGIN_ID
    };
    let mut config = ServeConfig::from_env(configured_id, PLUGIN_VERSION)?;
    if matches!(mode, Mode::SameId) {
        config.plugin_id = SHARED_PLUGIN_ID.to_owned();
    }
    config.plugin_name = PLUGIN_NAME.to_owned();
    config.capabilities.streaming_catalog = true;
    config.capabilities.streaming_suggestions = true;
    config.capabilities.cancellation = true;
    let mut plugin = ConformancePlugin::new(mode);
    serve(&mut plugin, config)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("crikey-conformance-plugin: {error:?}");
        std::process::exit(1);
    }
}
