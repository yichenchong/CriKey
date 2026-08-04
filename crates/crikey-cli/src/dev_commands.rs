//! Deterministic developer query tracing and typing simulation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::process::ExitCode;

use crikey_app::{
    BatchPriority, BatchState, DrainBudget, IntakePolicy, OverflowPolicy, PipelineConfig, PipelineError,
    QueryPipeline, QueueDiagnostics, QueueEvent, QueueEventKind, QueueLimits, QueueReject, RejectReason,
    ResultBatch,
};
use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_input_scheduler::{
    BatchCompletion, CancelReason, DebounceDecision, DebouncePolicy, DispatchedRequest, GateReason,
    LegacyDispatch, PluginPolicy, QueryTraceEvent, QueuePolicy, SchedulerConfig, SchedulingProfile,
};
use crikey_ui::ViewModel;

const EX_USAGE: u8 = 64;
const EX_SOFTWARE: u8 = 70;
const DEFAULT_INTERVAL_MS: u64 = 10;
const REQUEST_QUEUE_CAPACITY: usize = 8;
const RESULT_QUEUE_CAPACITY: usize = 8;
const DEBOUNCE_MS: u64 = 50;
const MAXIMUM_WAIT_MS: u64 = 200;
const TRACE_CAPACITY: usize = 65_536;
const MAX_QUERY_CHARS: usize = 256;
const MAX_WORKLOAD_KEYSTROKES: usize = 512;

const FIXTURE_NAMES: [&str; 4] = [
    "modern-debounce",
    "legacy-strict",
    "slow-and-fast",
    "rapid-typing",
];

const MODERN_PLUGIN: PluginSpec = PluginSpec {
    id: "modern.debounce",
    kind: PluginKind::ModernDebounced,
    first_result_ms: 20,
    final_result_ms: 40,
};
const LEGACY_PLUGIN: PluginSpec = PluginSpec {
    id: "legacy.strict",
    kind: PluginKind::LegacyStrict,
    first_result_ms: 15,
    final_result_ms: 45,
};
const FAST_PLUGIN: PluginSpec = PluginSpec {
    id: "modern.fast",
    kind: PluginKind::ModernPrompt,
    first_result_ms: 5,
    final_result_ms: 12,
};
const SLOW_PLUGIN: PluginSpec = PluginSpec {
    id: "modern.slow",
    kind: PluginKind::ModernPrompt,
    first_result_ms: 80,
    final_result_ms: 120,
};

const MODERN_PLUGINS: [PluginSpec; 1] = [MODERN_PLUGIN];
const LEGACY_PLUGINS: [PluginSpec; 1] = [LEGACY_PLUGIN];
const SLOW_AND_FAST_PLUGINS: [PluginSpec; 2] = [FAST_PLUGIN, SLOW_PLUGIN];
const RAPID_TYPING_PLUGINS: [PluginSpec; 4] = [MODERN_PLUGIN, LEGACY_PLUGIN, FAST_PLUGIN, SLOW_PLUGIN];

pub(crate) fn trace_query(args: &[String]) -> ExitCode {
    developer_command("trace-query", args, true)
}

pub(crate) fn simulate_typing(args: &[String]) -> ExitCode {
    developer_command("simulate-typing", args, false)
}

fn developer_command(command: &str, args: &[String], print_trace: bool) -> ExitCode {
    let request = match parse_args(args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("crikey: {message}\n\n{}", usage(command));
            return ExitCode::from(EX_USAGE);
        }
    };

    match request {
        Request::Help => {
            print!("{}", help_records(command));
            ExitCode::SUCCESS
        }
        Request::ListFixtures => {
            for fixture in FIXTURE_NAMES {
                println!("fixture={fixture}");
            }
            ExitCode::SUCCESS
        }
        Request::Run(options) => match simulate(&options) {
            Ok(simulation) => {
                if print_trace {
                    print!("{}", trace_lines(&simulation));
                }
                print!("{}", summary_lines(&options, &simulation));
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("crikey: developer query simulation failed: {message}");
                ExitCode::from(EX_SOFTWARE)
            }
        },
    }
}

fn usage(command: &str) -> String {
    format!(
        "crikey dev {command} --fixture NAME [--query TEXT] [--interval-ms N] [--repeat N]\n\
         crikey dev {command} --list-fixtures\n\
         limits: {MAX_QUERY_CHARS} query characters, {MAX_WORKLOAD_KEYSTROKES} keystrokes\n\
         fixtures: {}",
        FIXTURE_NAMES.join(", ")
    )
}

fn help_records(command: &str) -> String {
    format!(
        "command={command}\n\
         usage=crikey_dev_{command}_--fixture_NAME_[--query_TEXT]_[--interval-ms_N]_[--repeat_N]\n\
         fixtures={}\n\
         max_query_chars={MAX_QUERY_CHARS}\n\
         max_workload_keystrokes={MAX_WORKLOAD_KEYSTROKES}\n\
         output={}\n",
        FIXTURE_NAMES.join(","),
        if command == "trace-query" {
            "trace-and-summary"
        } else {
            "summary-only"
        }
    )
}

#[derive(Debug)]
enum Request {
    Help,
    ListFixtures,
    Run(Options),
}

#[derive(Debug)]
struct Options {
    fixture: Fixture,
    query: String,
    interval_ms: u64,
    repeat: usize,
}

fn parse_args(args: &[String]) -> Result<Request, String> {
    if args.iter().any(|arg| matches!(arg.as_str(), "-h" | "--help")) {
        if let Some(argument) = unknown_help_argument(args) {
            return Err(format!("unrecognized developer command argument `{argument}`"));
        }
        return Ok(Request::Help);
    }

    let mut fixture = None;
    let mut query = None;
    let mut interval_ms = DEFAULT_INTERVAL_MS;
    let mut repeat = 1usize;
    let mut list_fixtures = false;
    let mut fixture_seen = false;
    let mut query_seen = false;
    let mut interval_seen = false;
    let mut repeat_seen = false;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--list-fixtures" => {
                if list_fixtures {
                    return Err("`--list-fixtures` may only be given once".to_owned());
                }
                list_fixtures = true;
                index += 1;
            }
            "--fixture" => {
                if fixture_seen {
                    return Err("`--fixture` may only be given once".to_owned());
                }
                let value = required_value(args, index, "--fixture")?;
                fixture = Some(parse_fixture(value)?);
                fixture_seen = true;
                index += 2;
            }
            "--query" => {
                if query_seen {
                    return Err("`--query` may only be given once".to_owned());
                }
                let value = required_value(args, index, "--query")?;
                query = Some(value.to_owned());
                query_seen = true;
                index += 2;
            }
            "--interval-ms" => {
                if interval_seen {
                    return Err("`--interval-ms` may only be given once".to_owned());
                }
                let value = required_value(args, index, "--interval-ms")?;
                interval_ms = positive_u64("--interval-ms", value)?;
                interval_seen = true;
                index += 2;
            }
            "--repeat" => {
                if repeat_seen {
                    return Err("`--repeat` may only be given once".to_owned());
                }
                let value = required_value(args, index, "--repeat")?;
                repeat = positive_usize("--repeat", value)?;
                repeat_seen = true;
                index += 2;
            }
            other if other.starts_with("--fixture=") => {
                if fixture_seen {
                    return Err("`--fixture` may only be given once".to_owned());
                }
                fixture = Some(parse_fixture(&other["--fixture=".len()..])?);
                fixture_seen = true;
                index += 1;
            }
            other if other.starts_with("--query=") => {
                if query_seen {
                    return Err("`--query` may only be given once".to_owned());
                }
                query = Some(other["--query=".len()..].to_owned());
                query_seen = true;
                index += 1;
            }
            other if other.starts_with("--interval-ms=") => {
                if interval_seen {
                    return Err("`--interval-ms` may only be given once".to_owned());
                }
                interval_ms = positive_u64("--interval-ms", &other["--interval-ms=".len()..])?;
                interval_seen = true;
                index += 1;
            }
            other if other.starts_with("--repeat=") => {
                if repeat_seen {
                    return Err("`--repeat` may only be given once".to_owned());
                }
                repeat = positive_usize("--repeat", &other["--repeat=".len()..])?;
                repeat_seen = true;
                index += 1;
            }
            other => return Err(format!("unrecognized developer command argument `{other}`")),
        }
    }

    if list_fixtures {
        if fixture.is_some() || query.is_some() || interval_ms != DEFAULT_INTERVAL_MS || repeat != 1 {
            return Err("`--list-fixtures` cannot be combined with a simulation workload".to_owned());
        }
        return Ok(Request::ListFixtures);
    }

    let fixture = fixture.ok_or("`--fixture` is required")?;
    let query = query.unwrap_or_else(|| fixture.default_query().to_owned());
    if query.is_empty() {
        return Err("`--query` must contain at least one character".to_owned());
    }
    validate_workload_size(&query, repeat)?;

    Ok(Request::Run(Options {
        fixture,
        query,
        interval_ms,
        repeat,
    }))
}

fn unknown_help_argument(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        if matches!(argument, "-h" | "--help" | "--list-fixtures") {
            index += 1;
        } else if matches!(argument, "--fixture" | "--query" | "--interval-ms" | "--repeat") {
            let Some(value) = args.get(index + 1) else {
                return Some(argument);
            };
            if value.starts_with('-') {
                return Some(value);
            }
            index += 2;
        } else if argument.starts_with("--fixture=")
            || argument.starts_with("--query=")
            || argument.starts_with("--interval-ms=")
            || argument.starts_with("--repeat=")
        {
            index += 1;
        } else {
            return Some(argument);
        }
    }
    None
}

fn required_value<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, String> {
    let value = args
        .get(index + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("`{option}` needs a value"))?;
    if value.starts_with('-') {
        return Err(format!(
            "`{option}` needs a value, got flag-like argument `{value}`"
        ));
    }
    Ok(value)
}

fn positive_u64(option: &str, value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("`{option}` needs a positive whole number, got `{value}`"))?;
    if parsed == 0 {
        return Err(format!("`{option}` must be at least 1"));
    }
    Ok(parsed)
}

fn positive_usize(option: &str, value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("`{option}` needs a positive whole number, got `{value}`"))?;
    if parsed == 0 {
        return Err(format!("`{option}` must be at least 1"));
    }
    Ok(parsed)
}
fn validate_workload_size(query: &str, repeat: usize) -> Result<(), String> {
    let query_chars = query.chars().count();
    if query_chars > MAX_QUERY_CHARS {
        return Err(format!(
            "`--query` contains {query_chars} characters; the deterministic fixture limit is {MAX_QUERY_CHARS}"
        ));
    }
    let keystrokes = query_chars
        .checked_mul(repeat)
        .ok_or("the requested workload has too many keystrokes")?;
    if keystrokes > MAX_WORKLOAD_KEYSTROKES {
        return Err(format!(
            "the requested workload has {keystrokes} keystrokes; the deterministic fixture limit is \
             {MAX_WORKLOAD_KEYSTROKES}"
        ));
    }
    Ok(())
}

fn parse_fixture(value: &str) -> Result<Fixture, String> {
    Fixture::parse(value).ok_or_else(|| {
        format!(
            "unknown fixture `{value}`; available fixtures: {}",
            FIXTURE_NAMES.join(", ")
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fixture {
    ModernDebounce,
    LegacyStrict,
    SlowAndFast,
    RapidTyping,
}

impl Fixture {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "modern-debounce" => Some(Self::ModernDebounce),
            "legacy-strict" => Some(Self::LegacyStrict),
            "slow-and-fast" => Some(Self::SlowAndFast),
            "rapid-typing" => Some(Self::RapidTyping),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ModernDebounce => "modern-debounce",
            Self::LegacyStrict => "legacy-strict",
            Self::SlowAndFast => "slow-and-fast",
            Self::RapidTyping => "rapid-typing",
        }
    }

    fn default_query(self) -> &'static str {
        match self {
            Self::ModernDebounce | Self::LegacyStrict | Self::SlowAndFast => "fire",
            Self::RapidTyping => "rapidtyping",
        }
    }

    fn plugins(self) -> &'static [PluginSpec] {
        match self {
            Self::ModernDebounce => &MODERN_PLUGINS,
            Self::LegacyStrict => &LEGACY_PLUGINS,
            Self::SlowAndFast => &SLOW_AND_FAST_PLUGINS,
            Self::RapidTyping => &RAPID_TYPING_PLUGINS,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PluginSpec {
    id: &'static str,
    kind: PluginKind,
    first_result_ms: u64,
    final_result_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginKind {
    ModernDebounced,
    ModernPrompt,
    LegacyStrict,
}

impl PluginSpec {
    fn policy(self) -> PluginPolicy {
        match self.kind {
            PluginKind::ModernDebounced => PluginPolicy {
                debounce: DebouncePolicy {
                    debounce_ms: DEBOUNCE_MS,
                    maximum_wait_ms: Some(MAXIMUM_WAIT_MS),
                    leading_edge: false,
                    trailing_edge: true,
                    minimum_query_length: 0,
                },
                queue_policy: QueuePolicy::ReplaceOldest,
                queue_capacity: 1,
                max_concurrent_requests: 1,
                ..PluginPolicy::modern()
            },
            PluginKind::ModernPrompt => PluginPolicy {
                debounce: DebouncePolicy {
                    debounce_ms: 0,
                    maximum_wait_ms: None,
                    leading_edge: true,
                    trailing_edge: true,
                    minimum_query_length: 0,
                },
                queue_policy: QueuePolicy::ReplaceOldest,
                queue_capacity: 1,
                max_concurrent_requests: 1,
                ..PluginPolicy::modern()
            },
            PluginKind::LegacyStrict => PluginPolicy {
                queue_policy: QueuePolicy::DropOldest,
                queue_capacity: 1,
                max_concurrent_requests: 1,
                ..PluginPolicy::legacy_strict()
            },
        }
    }

    fn intake_policy(self, fixture: Fixture) -> IntakePolicy {
        let capacity_batches = if fixture == Fixture::RapidTyping { 1 } else { 2 };
        IntakePolicy {
            capacity_batches,
            capacity_items: 8,
            pause_at_batches: capacity_batches,
            resume_at_batches: capacity_batches.saturating_sub(1),
            overflow: match (self.kind, self.id) {
                (PluginKind::ModernDebounced, _) => OverflowPolicy::RejectLowPriority,
                (PluginKind::ModernPrompt, "modern.fast") => OverflowPolicy::ReplaceOldest,
                (PluginKind::ModernPrompt | PluginKind::LegacyStrict, _) => OverflowPolicy::PauseProducer,
            },
        }
    }
}

const SETTLE_ROUNDS: usize = 16;

struct Simulation {
    pipeline: QueryPipeline,
    trace: TraceCapture,
    frames: Vec<FrameObservation>,
    generations: usize,
}

#[derive(Debug, Clone)]
enum ObservedEvent {
    Scheduler(QueryTraceEvent),
    Intake(QueueEvent),
    Frame(FrameObservation),
}

#[derive(Debug, Default)]
struct TraceCapture {
    events: Vec<ObservedEvent>,
    scheduler_seen: usize,
    scheduler_dropped_seen: u64,
}

impl TraceCapture {
    fn capture(&mut self, pipeline: &mut QueryPipeline) -> Result<(), String> {
        let diagnostics = pipeline.diagnostics();
        if diagnostics.trace_events_dropped > self.scheduler_dropped_seen {
            return Err(
                "the bounded scheduler trace dropped events before the fixture was observed".to_owned(),
            );
        }
        self.scheduler_dropped_seen = diagnostics.trace_events_dropped;

        self.events.extend(
            pipeline
                .take_intake_events()
                .into_iter()
                .map(ObservedEvent::Intake),
        );

        let scheduler_trace = pipeline.trace();
        if self.scheduler_seen > scheduler_trace.len() {
            return Err("the bounded scheduler trace rotated between fixture observations".to_owned());
        }
        self.events.extend(
            scheduler_trace[self.scheduler_seen..]
                .iter()
                .cloned()
                .map(ObservedEvent::Scheduler),
        );
        self.scheduler_seen = scheduler_trace.len();
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct FrameObservation {
    at: u64,
    generation: Generation,
    visible_items: usize,
    stale_items: usize,
    pending_plugins: bool,
}

impl FrameObservation {
    fn capture(at: u64, view: &ViewModel) -> Result<Self, String> {
        let mut stale_items = 0usize;
        for row in view.rows.iter() {
            let row_generation = fixture_row_generation(&row.item)?;
            if row_generation != view.generation.get() {
                stale_items = stale_items.saturating_add(1);
            }
        }
        Ok(Self {
            at,
            generation: view.generation,
            visible_items: view.rows.len(),
            stale_items,
            pending_plugins: view.pending_plugins,
        })
    }
}

#[derive(Debug, Clone)]
struct ScheduledResult {
    plugin: PluginId,
    generation: Generation,
    offset: usize,
    items: usize,
    state: BatchState,
    priority: BatchPriority,
}

fn simulate(options: &Options) -> Result<Simulation, String> {
    let plugin_budget = options.fixture.plugins().len().max(1);
    let config = PipelineConfig {
        scheduler: SchedulerConfig {
            request_queue_capacity: REQUEST_QUEUE_CAPACITY,
            result_queue_capacity: RESULT_QUEUE_CAPACITY,
            per_plugin_dispatch_budget: plugin_budget,
            dispatch_budget_per_tick: plugin_budget,
            trace_capacity: TRACE_CAPACITY,
        },
        intake_limits: QueueLimits {
            capacity_batches: RESULT_QUEUE_CAPACITY,
            capacity_items: 64,
        },
        drain_budget: DrainBudget {
            batches_per_plugin: 1,
            items_per_plugin: 8,
            total_batches: plugin_budget,
        },
        ..PipelineConfig::default()
    };

    let mut pipeline = QueryPipeline::new(config);
    for plugin in options.fixture.plugins() {
        pipeline
            .register_plugin_with_intake(
                PluginId(plugin.id.to_owned()),
                plugin.policy(),
                plugin.intake_policy(options.fixture),
            )
            .map_err(|error| format!("fixture plugin registration failed: {error}"))?;
    }

    let keystrokes = workload(options)?;
    let mut next_keystroke = 0usize;
    let mut scheduled: BTreeMap<u64, Vec<ScheduledResult>> = BTreeMap::new();
    let mut trace = TraceCapture::default();
    let mut frames = Vec::with_capacity(keystrokes.len().saturating_mul(2));
    let mut generations = 0usize;
    let mut now = 0u64;

    loop {
        let next_time = [
            keystrokes.get(next_keystroke).map(|stroke| stroke.0),
            scheduled.first_key_value().map(|(at, _)| *at),
            pipeline.next_wakeup(),
        ]
        .into_iter()
        .flatten()
        .min();

        let Some(candidate) = next_time else {
            let diagnostics = pipeline.diagnostics();
            let intake = pipeline.intake_depth();
            if diagnostics.queued_requests == 0 && diagnostics.in_flight_requests == 0 && intake.batches == 0
            {
                break;
            }
            return Err(format!(
                "the virtual runtime retained queued={}, in_flight={}, intake={} without a result or wake-up",
                diagnostics.queued_requests, diagnostics.in_flight_requests, intake.batches
            ));
        };
        now = candidate.max(now);

        while keystrokes
            .get(next_keystroke)
            .is_some_and(|stroke| stroke.0 <= now)
        {
            let (_, query) = &keystrokes[next_keystroke];
            pipeline.keystroke(query, now);
            generations = generations.saturating_add(1);
            next_keystroke += 1;
            trace.capture(&mut pipeline)?;
        }

        while scheduled.first_key_value().is_some_and(|(at, _)| *at <= now) {
            let (_, results) = scheduled
                .pop_first()
                .expect("the scheduled result queue was checked as non-empty");
            for result in results {
                deliver_result(&mut pipeline, &mut trace, result, now)?;
            }
        }

        settle(
            options.fixture,
            &mut pipeline,
            &mut trace,
            &mut scheduled,
            &mut now,
        )?;
        present_and_capture(&mut pipeline, &mut trace, &mut frames, now)?;

        if pipeline.next_wakeup().is_some_and(|wake| wake <= now) {
            return Err(format!(
                "the virtual runtime retained an overdue scheduler wake-up at {now}ms"
            ));
        }
    }

    trace.capture(&mut pipeline)?;
    Ok(Simulation {
        pipeline,
        trace,
        frames,
        generations,
    })
}

fn workload(options: &Options) -> Result<Vec<(u64, String)>, String> {
    let chars: Vec<char> = options.query.chars().collect();
    let strokes = chars
        .len()
        .checked_mul(options.repeat)
        .ok_or("the requested workload has too many keystrokes")?;
    let mut workload = Vec::with_capacity(strokes);
    let mut ordinal = 0usize;

    for _ in 0..options.repeat {
        let mut prefix = String::new();
        for character in &chars {
            prefix.push(*character);
            let ordinal_ms =
                u64::try_from(ordinal).map_err(|_| "the requested workload exceeds virtual clock range")?;
            let at = ordinal_ms
                .checked_mul(options.interval_ms)
                .ok_or("the requested workload exceeds virtual clock range")?;
            workload.push((at, prefix.clone()));
            ordinal = ordinal
                .checked_add(1)
                .ok_or("the requested workload has too many keystrokes")?;
        }
    }
    Ok(workload)
}

fn settle(
    fixture: Fixture,
    pipeline: &mut QueryPipeline,
    trace: &mut TraceCapture,
    scheduled: &mut BTreeMap<u64, Vec<ScheduledResult>>,
    now: &mut u64,
) -> Result<(), String> {
    for round in 0..SETTLE_ROUNDS {
        if round != 0 {
            *now = now
                .checked_add(1)
                .ok_or("the simulated pipeline service clock exceeded virtual clock range")?;
        }
        let tick = pipeline.tick(*now);
        let progressed = !tick.dispatches.is_empty()
            || !tick.cancellations.is_empty()
            || tick.drain_report.merged != 0
            || tick.drain_report.dropped_obsolete != 0;
        trace.capture(pipeline)?;
        if !tick.errors.is_empty() {
            return Err(format!(
                "pipeline tick rejected fixture work: {}",
                pipeline_error_text(&tick.errors)
            ));
        }
        schedule_results(fixture, tick.dispatches, scheduled)?;
        if !progressed {
            return Ok(());
        }
    }
    Err(format!(
        "the query pipeline did not settle within {SETTLE_ROUNDS} rounds at {now}ms"
    ))
}

fn pipeline_error_text(errors: &[PipelineError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn present_and_capture(
    pipeline: &mut QueryPipeline,
    trace: &mut TraceCapture,
    frames: &mut Vec<FrameObservation>,
    now: u64,
) -> Result<(), String> {
    let view = pipeline.present(now);
    trace.capture(pipeline)?;
    let errors = pipeline.take_errors();
    if !errors.is_empty() {
        return Err(format!(
            "pipeline presentation rejected fixture work: {}",
            pipeline_error_text(&errors)
        ));
    }
    if let Some(view) = view {
        let frame = FrameObservation::capture(now, &view)?;
        trace.events.push(ObservedEvent::Frame(frame.clone()));
        frames.push(frame);
    }
    Ok(())
}

fn schedule_results(
    fixture: Fixture,
    dispatched: Vec<DispatchedRequest>,
    scheduled: &mut BTreeMap<u64, Vec<ScheduledResult>>,
) -> Result<(), String> {
    for request in dispatched {
        let plugin = fixture
            .plugins()
            .iter()
            .find(|plugin| plugin.id == request.plugin.0)
            .ok_or_else(|| format!("scheduler dispatched unknown plugin `{}`", request.plugin.0))?;
        let mut publications = Vec::with_capacity(3);
        publications.push((
            plugin.first_result_ms,
            0,
            2,
            BatchState::Partial,
            BatchPriority::High,
        ));
        if fixture == Fixture::RapidTyping {
            // A second publication before the first drain is a real,
            // per-producer overflow probe. Its outcome is decided by the
            // configured intake policy, never filled in by the fixture.
            publications.push((
                plugin.first_result_ms,
                2,
                1,
                BatchState::Partial,
                BatchPriority::Low,
            ));
        }
        publications.push((
            plugin.final_result_ms,
            if fixture == Fixture::RapidTyping { 3 } else { 2 },
            1,
            BatchState::Final,
            BatchPriority::High,
        ));

        for (latency, offset, items, state, priority) in publications {
            let at = request
                .dispatched_at
                .checked_add(latency)
                .ok_or("a simulated plugin result exceeded virtual clock range")?;
            scheduled.entry(at).or_default().push(ScheduledResult {
                plugin: request.plugin.clone(),
                generation: request.generation,
                offset,
                items,
                state,
                priority,
            });
        }
    }
    Ok(())
}

fn deliver_result(
    pipeline: &mut QueryPipeline,
    trace: &mut TraceCapture,
    result: ScheduledResult,
    now: u64,
) -> Result<(), String> {
    let terminal = result.state != BatchState::Partial;
    let plugin = result.plugin.clone();
    let generation = result.generation;
    let delivery = pipeline.deliver_with_priority(
        ResultBatch {
            generation,
            plugin: plugin.clone(),
            state: result.state,
            items: fixture_items(&result),
        },
        result.priority,
        now,
    );
    trace.capture(pipeline)?;

    match delivery {
        Ok(()) => {}
        Err(PipelineError::QueueRejected { reason, .. })
            if !terminal
                && matches!(
                    reason,
                    QueueReject::StaleGeneration
                        | QueueReject::LowPriorityShed
                        | QueueReject::QueueFull
                        | QueueReject::BoundaryFull
                        | QueueReject::Disconnected
                ) => {}
        Err(PipelineError::QueueRejected {
            reason: QueueReject::StaleGeneration,
            ..
        }) => {}
        Err(error) => {
            return Err(format!("fixture result delivery failed: {error}"));
        }
    }

    if terminal {
        let _ = pipeline.complete(&plugin, generation, now);
        trace.capture(pipeline)?;
    }
    Ok(())
}

fn fixture_items(result: &ScheduledResult) -> Vec<Item> {
    (result.offset..result.offset + result.items)
        .map(|index| Item {
            stable_id: ItemId(format!(
                "{}#g{}#{index}",
                result.plugin.0,
                result.generation.get()
            )),
            plugin_id: result.plugin.clone(),
            category: Category::Application,
            label: format!("{} answer {index}", result.plugin.0),
            description: format!("fixture answer for generation {}", result.generation.get()),
            target: format!("fixture://{}/{index}", result.plugin.0),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 1_000i32.saturating_sub(i32::try_from(index).unwrap_or(i32::MAX)),
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        })
        .collect()
}

fn fixture_row_generation(item: &ItemId) -> Result<u64, String> {
    let (_, encoded) = item
        .0
        .rsplit_once("#g")
        .ok_or_else(|| format!("fixture row `{}` lost its generation marker", item.0))?;
    let (digits, _) = encoded
        .split_once('#')
        .ok_or_else(|| format!("fixture row `{}` has an incomplete generation marker", item.0))?;
    digits
        .parse()
        .map_err(|_| format!("fixture row `{}` has a non-numeric generation", item.0))
}

fn trace_lines(simulation: &Simulation) -> String {
    let mut output = String::new();
    for event in &simulation.trace.events {
        match event {
            ObservedEvent::Scheduler(event) => render_scheduler_event(&mut output, event),
            ObservedEvent::Intake(event) => render_intake_event(&mut output, event),
            ObservedEvent::Frame(frame) => line(
                &mut output,
                format_args!(
                    "event=frame at_ms={} generation={} visible_items={} stale_items={} pending_plugins={}",
                    frame.at,
                    frame.generation.get(),
                    frame.visible_items,
                    frame.stale_items,
                    usize::from(frame.pending_plugins)
                ),
            ),
        }
    }
    output
}

fn render_scheduler_event(output: &mut String, event: &QueryTraceEvent) {
    match event {
            QueryTraceEvent::Keystroke {
                at,
                generation,
                query_length,
            } => line(
                output,
                format_args!(
                    "event=keystroke at_ms={at} generation={} query_length={query_length}",
                    generation.get()
                ),
            ),
            QueryTraceEvent::Debounce {
                at,
                plugin,
                generation,
                decision,
            } => match decision {
                DebounceDecision::LeadingEdge => line(
                    output,
                    format_args!(
                        "event=debounce at_ms={at} generation={} plugin={} decision=leading-edge",
                        generation.get(),
                        plugin.0
                    ),
                ),
                DebounceDecision::TrailingEdge => line(
                    output,
                    format_args!(
                        "event=debounce at_ms={at} generation={} plugin={} decision=trailing-edge",
                        generation.get(),
                        plugin.0
                    ),
                ),
                DebounceDecision::MaximumWait => line(
                    output,
                    format_args!(
                        "event=debounce at_ms={at} generation={} plugin={} decision=maximum-wait",
                        generation.get(),
                        plugin.0
                    ),
                ),
                DebounceDecision::Deferred { until } => line(
                    output,
                    format_args!(
                        "event=debounce at_ms={at} generation={} plugin={} decision=deferred until_ms={until}",
                        generation.get(),
                        plugin.0
                    ),
                ),
                DebounceDecision::Coalesced { superseded } => line(
                    output,
                    format_args!(
                        "event=debounce at_ms={at} generation={} plugin={} decision=coalesced superseded_generation={}",
                        generation.get(),
                        plugin.0,
                        superseded.get()
                    ),
                ),
                DebounceDecision::Gated(reason) => line(
                    output,
                    format_args!(
                        "event=debounce at_ms={at} generation={} plugin={} decision=gated reason={}",
                        generation.get(),
                        plugin.0,
                        gate_reason(*reason)
                    ),
                ),
            },
            QueryTraceEvent::LegacyDispatch {
                at,
                plugin,
                generation,
                decision,
            } => match decision {
                LegacyDispatch::Now(dispatched) => line(
                    output,
                    format_args!(
                        "event=legacy_dispatch at_ms={at} generation={} plugin={} decision=now dispatched_generation={}",
                        generation.get(),
                        plugin.0,
                        dispatched.get()
                    ),
                ),
                LegacyDispatch::QueuedBehindRunning { obsolete, queued } => line(
                    output,
                    format_args!(
                        "event=legacy_dispatch at_ms={at} generation={} plugin={} decision=queued-behind-running obsolete_generation={} queued_generation={}",
                        generation.get(),
                        plugin.0,
                        obsolete.get(),
                        queued.get()
                    ),
                ),
                LegacyDispatch::Idle => line(
                    output,
                    format_args!(
                        "event=legacy_dispatch at_ms={at} generation={} plugin={} decision=idle",
                        generation.get(),
                        plugin.0
                    ),
                ),
            },
            QueryTraceEvent::Dispatched {
                at,
                plugin,
                generation,
            } => line(
                output,
                format_args!(
                    "event=dispatched at_ms={at} generation={} plugin={}",
                    generation.get(),
                    plugin.0
                ),
            ),
            QueryTraceEvent::RequestDropped {
                at,
                plugin,
                generation,
                policy,
            } => line(
                output,
                format_args!(
                    "event=request_dropped at_ms={at} generation={} plugin={} policy={}",
                    generation.get(),
                    plugin.0,
                    queue_policy(*policy)
                ),
            ),
            QueryTraceEvent::Cancelled {
                at,
                plugin,
                generation,
                reason,
            } => line(
                output,
                format_args!(
                    "event=cancelled at_ms={at} generation={} plugin={} reason={}",
                    generation.get(),
                    plugin.0,
                    cancel_reason(*reason)
                ),
            ),
            QueryTraceEvent::FirstResult {
                at,
                plugin,
                generation,
                latency_ms,
            } => line(
                output,
                format_args!(
                    "event=first_result at_ms={at} generation={} plugin={} latency_ms={latency_ms}",
                    generation.get(),
                    plugin.0
                ),
            ),
            QueryTraceEvent::FinalResult {
                at,
                plugin,
                generation,
                latency_ms,
            } => line(
                output,
                format_args!(
                    "event=final_result at_ms={at} generation={} plugin={} latency_ms={latency_ms}",
                    generation.get(),
                    plugin.0
                ),
            ),
            QueryTraceEvent::ResultBatch {
                at,
                plugin,
                generation,
                items,
                completion,
            } => line(
                output,
                format_args!(
                    "event=result_batch at_ms={at} generation={} plugin={} items={items} completion={}",
                    generation.get(),
                    plugin.0,
                    batch_completion(*completion)
                ),
            ),
            QueryTraceEvent::StaleResultRejected {
                at,
                plugin,
                generation,
            } => line(
                output,
                format_args!(
                    "event=stale_result_rejected at_ms={at} generation={} plugin={}",
                    generation.get(),
                    plugin.0
                ),
            ),
            QueryTraceEvent::Ranking {
                at,
                generation,
                ranked_items,
            } => line(
                output,
                format_args!(
                    "event=ranking at_ms={at} generation={} ranked_items={ranked_items}",
                    generation.get()
                ),
            ),
            QueryTraceEvent::Presentation {
                at,
                generation,
                visible_items,
            } => line(
                output,
                format_args!(
                    "event=presentation at_ms={at} generation={} visible_items={visible_items}",
                    generation.get()
                ),
            ),
        }
}

fn render_intake_event(output: &mut String, event: &QueueEvent) {
    let at = event.at_ms;
    let generation = event.generation.get();
    let plugin = &event.plugin.0;
    match &event.kind {
        QueueEventKind::Admitted { items } => line(
            output,
            format_args!(
                "event=result_queue_admitted at_ms={at} generation={generation} plugin={plugin} items={items}"
            ),
        ),
        QueueEventKind::Rejected(reason) => line(
            output,
            format_args!(
                "event=result_queue_rejected at_ms={at} generation={generation} plugin={plugin} reason={}",
                queue_reject(*reason)
            ),
        ),
        QueueEventKind::DroppedObsolete { batches, items } => line(
            output,
            format_args!(
                "event=result_queue_dropped_obsolete at_ms={at} generation={generation} plugin={plugin} batches={batches} items={items}"
            ),
        ),
        QueueEventKind::EvictedOldest { items } => line(
            output,
            format_args!(
                "event=result_queue_evicted at_ms={at} generation={generation} plugin={plugin} items={items}"
            ),
        ),
        QueueEventKind::Merged { items } => line(
            output,
            format_args!(
                "event=result_queue_merged at_ms={at} generation={generation} plugin={plugin} items={items}"
            ),
        ),
        QueueEventKind::MergeRejected(reason) => line(
            output,
            format_args!(
                "event=result_merge_rejected at_ms={at} generation={generation} plugin={plugin} reason={}",
                reject_reason(*reason)
            ),
        ),
        QueueEventKind::ProducerPaused => line(
            output,
            format_args!(
                "event=producer_paused at_ms={at} generation={generation} plugin={plugin}"
            ),
        ),
        QueueEventKind::ProducerResumed => line(
            output,
            format_args!(
                "event=producer_resumed at_ms={at} generation={generation} plugin={plugin}"
            ),
        ),
    }
}

fn summary_lines(options: &Options, simulation: &Simulation) -> String {
    let diagnostics = simulation.pipeline.diagnostics();
    let intake = simulation.pipeline.intake_diagnostics();
    let mut first_latencies = Vec::new();
    let mut final_latencies = Vec::new();
    let mut first_result_by_plugin: BTreeMap<&str, u64> = BTreeMap::new();

    for event in &simulation.trace.events {
        let ObservedEvent::Scheduler(event) = event else {
            continue;
        };
        match event {
            QueryTraceEvent::FirstResult {
                at,
                plugin,
                latency_ms,
                ..
            } => {
                first_latencies.push(*latency_ms);
                first_result_by_plugin.entry(&plugin.0).or_insert(*at);
            }
            QueryTraceEvent::FinalResult { latency_ms, .. } => final_latencies.push(*latency_ms),
            _ => {}
        }
    }

    let first_result_latency_ms = first_latencies.into_iter().min().unwrap_or(0);
    let final_result_latency_ms = final_latencies.into_iter().max().unwrap_or(0);
    let fastest_plugin_first_result_ms = first_result_by_plugin.values().copied().min().unwrap_or(0);
    let slowest_plugin_first_result_ms = first_result_by_plugin.values().copied().max().unwrap_or(0);
    let presentations_before_slowest_first_result = simulation
        .frames
        .iter()
        .filter(|frame| frame.at < slowest_plugin_first_result_ms && frame.visible_items > 0)
        .count();
    let cross_generation_reorderings = simulation
        .frames
        .windows(2)
        .filter(|pair| pair[1].generation < pair[0].generation)
        .count();
    let stale_results_displayed = simulation
        .frames
        .iter()
        .fold(0usize, |total, frame| total.saturating_add(frame.stale_items));
    let presented_items = simulation
        .frames
        .iter()
        .fold(0usize, |total, frame| total.saturating_add(frame.visible_items));

    let mut request_policies = BTreeSet::new();
    let mut result_policies = BTreeSet::new();
    let mut max_pending_per_plugin = 0usize;
    let mut effective_legacy_policies = BTreeSet::new();
    let mut effective_legacy_queue_capacity = 0usize;
    let mut effective_legacy_max_concurrent_requests = 0usize;
    let mut legacy_coalesced_requests = 0u64;
    let mut legacy_dropped_obsolete_requests = 0u64;
    let mut legacy_rejected_queue_full = 0u64;
    let mut legacy_dispatched_requests = 0u64;
    let mut legacy_cancelled_requests = 0u64;
    let mut legacy_rejected_stale_results = 0u64;

    for plugin in options.fixture.plugins() {
        let id = PluginId(plugin.id.to_owned());
        if let Some(policy) = simulation.pipeline.plugin_policy(&id) {
            request_policies.insert(queue_policy(policy.queue_policy));
            if policy.profile == SchedulingProfile::LegacyStrict {
                effective_legacy_policies.insert(queue_policy(policy.queue_policy));
                effective_legacy_queue_capacity = effective_legacy_queue_capacity.max(policy.queue_capacity);
                effective_legacy_max_concurrent_requests =
                    effective_legacy_max_concurrent_requests.max(policy.max_concurrent_requests);
            }
        }
        result_policies.insert(overflow_policy(plugin.intake_policy(options.fixture).overflow));
        if let Some(plugin_diagnostics) = simulation.pipeline.plugin_diagnostics(&id) {
            max_pending_per_plugin = max_pending_per_plugin.max(plugin_diagnostics.peak_queue_depth);
            if plugin.kind == PluginKind::LegacyStrict {
                legacy_coalesced_requests =
                    legacy_coalesced_requests.saturating_add(plugin_diagnostics.coalesced_requests);
                legacy_dropped_obsolete_requests = legacy_dropped_obsolete_requests
                    .saturating_add(plugin_diagnostics.dropped_obsolete_requests);
                legacy_rejected_queue_full =
                    legacy_rejected_queue_full.saturating_add(plugin_diagnostics.rejected_queue_full);
                legacy_dispatched_requests =
                    legacy_dispatched_requests.saturating_add(plugin_diagnostics.dispatched_requests);
                legacy_cancelled_requests =
                    legacy_cancelled_requests.saturating_add(plugin_diagnostics.cancelled_requests);
                legacy_rejected_stale_results =
                    legacy_rejected_stale_results.saturating_add(plugin_diagnostics.rejected_stale_results);
            }
        }
    }

    let request_policies = request_policies.into_iter().collect::<Vec<_>>().join(",");
    let result_policies = result_policies.into_iter().collect::<Vec<_>>().join(",");
    let effective_legacy_queue_policy = if effective_legacy_policies.is_empty() {
        "none".to_owned()
    } else {
        effective_legacy_policies
            .into_iter()
            .collect::<Vec<_>>()
            .join(",")
    };
    let result_batches_rejected = queue_rejection_total(intake);
    let result_merge_rejected = merge_rejection_total(intake);
    let trace_truncated = usize::from(
        diagnostics.trace_events_dropped != 0
            || intake.events_dropped() != 0
            || simulation.pipeline.dropped_errors() != 0,
    );

    let mut output = String::new();
    summary(&mut output, "fixture", options.fixture.name());
    summary(&mut output, "keystrokes", simulation.generations);
    summary(&mut output, "generations", simulation.generations);
    summary(&mut output, "plugins", options.fixture.plugins().len());
    summary(&mut output, "workload_keystroke_limit", MAX_WORKLOAD_KEYSTROKES);
    summary(&mut output, "request_queue_capacity", REQUEST_QUEUE_CAPACITY);
    summary(&mut output, "peak_queue_depth", diagnostics.peak_queue_depth);
    summary(&mut output, "result_queue_capacity", RESULT_QUEUE_CAPACITY);
    summary(&mut output, "peak_result_queue_depth", intake.peak_batches());
    summary(&mut output, "request_queue_overflow_policy", request_policies);
    summary(&mut output, "result_queue_overflow_policy", result_policies);
    summary(&mut output, "max_pending_per_plugin", max_pending_per_plugin);
    summary(&mut output, "coalesced_requests", diagnostics.coalesced_requests);
    summary(
        &mut output,
        "dropped_obsolete_requests",
        diagnostics.dropped_obsolete_requests,
    );
    summary(
        &mut output,
        "rejected_plugin_queue_full",
        diagnostics.rejected_plugin_queue_full,
    );
    summary(
        &mut output,
        "rejected_global_queue_full",
        diagnostics.rejected_global_queue_full,
    );
    summary(
        &mut output,
        "discarded_requests",
        diagnostics.discarded_requests(),
    );
    summary(
        &mut output,
        "dispatched_requests",
        diagnostics.dispatched_requests,
    );
    summary(&mut output, "cancelled_requests", diagnostics.cancelled_requests);
    summary(
        &mut output,
        "rejected_stale_results",
        diagnostics.rejected_stale_results,
    );
    summary(&mut output, "stale_results_displayed", stale_results_displayed);
    summary(
        &mut output,
        "cross_generation_reorderings",
        cross_generation_reorderings,
    );
    summary(&mut output, "presented_frames", simulation.frames.len());
    summary(&mut output, "presented_items", presented_items);
    summary(&mut output, "first_result_latency_ms", first_result_latency_ms);
    summary(&mut output, "final_result_latency_ms", final_result_latency_ms);
    summary(&mut output, "debounce_ms", DEBOUNCE_MS);
    summary(&mut output, "maximum_wait_ms", MAXIMUM_WAIT_MS);
    summary(
        &mut output,
        "fastest_plugin_first_result_ms",
        fastest_plugin_first_result_ms,
    );
    summary(
        &mut output,
        "slowest_plugin_first_result_ms",
        slowest_plugin_first_result_ms,
    );
    summary(
        &mut output,
        "presentations_before_slowest_first_result",
        presentations_before_slowest_first_result,
    );
    summary(&mut output, "trace_capacity", TRACE_CAPACITY);
    summary(
        &mut output,
        "trace_events_dropped",
        diagnostics.trace_events_dropped,
    );
    summary(&mut output, "intake_events_dropped", intake.events_dropped());
    summary(
        &mut output,
        "pipeline_errors_dropped",
        simulation.pipeline.dropped_errors(),
    );
    summary(&mut output, "trace_truncated", trace_truncated);
    summary(&mut output, "result_batches_admitted", intake.admitted());
    summary(&mut output, "result_batches_merged", intake.merged());
    summary(&mut output, "result_batches_rejected", result_batches_rejected);
    summary(&mut output, "result_merge_rejected", result_merge_rejected);
    summary(&mut output, "result_batches_evicted", intake.evicted_oldest());
    summary(
        &mut output,
        "result_obsolete_batches_dropped",
        intake.dropped_obsolete(),
    );
    summary(&mut output, "result_producer_pauses", intake.pauses());
    summary(&mut output, "result_producer_resumes", intake.resumes());
    summary(
        &mut output,
        "effective_legacy_queue_policy",
        effective_legacy_queue_policy,
    );
    summary(
        &mut output,
        "effective_legacy_queue_capacity",
        effective_legacy_queue_capacity,
    );
    summary(
        &mut output,
        "effective_legacy_max_concurrent_requests",
        effective_legacy_max_concurrent_requests,
    );
    summary(
        &mut output,
        "legacy_coalesced_requests",
        legacy_coalesced_requests,
    );
    summary(
        &mut output,
        "legacy_dropped_obsolete_requests",
        legacy_dropped_obsolete_requests,
    );
    summary(
        &mut output,
        "legacy_rejected_queue_full",
        legacy_rejected_queue_full,
    );
    summary(
        &mut output,
        "legacy_dispatched_requests",
        legacy_dispatched_requests,
    );
    summary(
        &mut output,
        "legacy_cancelled_requests",
        legacy_cancelled_requests,
    );
    summary(
        &mut output,
        "legacy_rejected_stale_results",
        legacy_rejected_stale_results,
    );
    output
}

fn queue_rejection_total(diagnostics: &QueueDiagnostics) -> usize {
    [
        QueueReject::Unregistered,
        QueueReject::StaleGeneration,
        QueueReject::StreamTerminated,
        QueueReject::LowPriorityShed,
        QueueReject::QueueFull,
        QueueReject::BoundaryFull,
        QueueReject::Disconnected,
    ]
    .into_iter()
    .fold(0usize, |total, reason| {
        total.saturating_add(diagnostics.rejected(reason))
    })
}

fn merge_rejection_total(diagnostics: &QueueDiagnostics) -> usize {
    [
        RejectReason::StaleGeneration,
        RejectReason::QuotaExceeded,
        RejectReason::PayloadTooLarge,
        RejectReason::OwnerMismatch,
        RejectReason::StreamTerminated,
        RejectReason::PluginSuspended,
    ]
    .into_iter()
    .fold(0usize, |total, reason| {
        total.saturating_add(diagnostics.merge_rejected(reason))
    })
}

fn line(output: &mut String, fields: std::fmt::Arguments<'_>) {
    output.write_fmt(fields).expect("writing to a String cannot fail");
    output.push('\n');
}

fn summary(output: &mut String, key: &str, value: impl std::fmt::Display) {
    writeln!(output, "{key}={value}").expect("writing to a String cannot fail");
}

fn queue_policy(policy: QueuePolicy) -> &'static str {
    match policy {
        QueuePolicy::ReplaceOldest => "replace-oldest",
        QueuePolicy::RejectNewest => "reject-newest",
        QueuePolicy::DropOldest => "drop-oldest",
    }
}
fn overflow_policy(policy: OverflowPolicy) -> &'static str {
    match policy {
        OverflowPolicy::RejectLowPriority => "reject-low-priority",
        OverflowPolicy::PauseProducer => "pause-producer",
        OverflowPolicy::ReplaceOldest => "replace-oldest",
        OverflowPolicy::Disconnect => "disconnect",
    }
}

fn queue_reject(reason: QueueReject) -> &'static str {
    match reason {
        QueueReject::Unregistered => "unregistered",
        QueueReject::StaleGeneration => "stale-generation",
        QueueReject::StreamTerminated => "stream-terminated",
        QueueReject::LowPriorityShed => "low-priority-shed",
        QueueReject::QueueFull => "queue-full",
        QueueReject::BoundaryFull => "boundary-full",
        QueueReject::Disconnected => "disconnected",
    }
}

fn reject_reason(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::StaleGeneration => "stale-generation",
        RejectReason::QuotaExceeded => "quota-exceeded",
        RejectReason::PayloadTooLarge => "payload-too-large",
        RejectReason::OwnerMismatch => "owner-mismatch",
        RejectReason::StreamTerminated => "stream-terminated",
        RejectReason::PluginSuspended => "plugin-suspended",
    }
}

fn batch_completion(completion: BatchCompletion) -> &'static str {
    match completion {
        BatchCompletion::Partial => "partial",
        BatchCompletion::Final => "final",
        BatchCompletion::Cancelled => "cancelled",
        BatchCompletion::Failed => "failed",
    }
}

fn gate_reason(reason: GateReason) -> &'static str {
    match reason {
        GateReason::MinimumQueryLength => "minimum-query-length",
        GateReason::EmptyQueryUnsupported => "empty-query-unsupported",
        GateReason::PrefixMismatch => "prefix-mismatch",
        GateReason::KeywordMismatch => "keyword-mismatch",
        GateReason::Disabled => "disabled",
    }
}

fn cancel_reason(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::QueryChanged => "query-changed",
        CancelReason::NoLongerRelevant => "no-longer-relevant",
        CancelReason::Reconfigured => "reconfigured",
        CancelReason::ProfileChanged => "profile-changed",
        CancelReason::Disabled => "disabled",
        CancelReason::Shutdown => "shutdown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_does_not_hide_unknown_developer_options() {
        let args = vec!["--help".to_owned(), "--unknown".to_owned()];
        assert!(parse_args(&args).is_err());
    }
    #[test]
    fn separate_values_cannot_consume_the_next_flag() {
        let query = vec![
            "--fixture".to_owned(),
            "rapid-typing".to_owned(),
            "--query".to_owned(),
            "--unknown".to_owned(),
        ];
        assert!(parse_args(&query).is_err());

        let fixture = vec!["--fixture".to_owned(), "--query".to_owned(), "--help".to_owned()];
        assert!(parse_args(&fixture).is_err());
        assert!(parse_args(&["--help".to_owned(), "--interval-ms".to_owned()]).is_err());
    }

    #[test]
    fn scalar_options_cannot_be_silently_replaced() {
        let duplicate_fixture = vec![
            "--fixture".to_owned(),
            "rapid-typing".to_owned(),
            "--fixture=modern-debounce".to_owned(),
        ];
        assert!(parse_args(&duplicate_fixture).is_err());

        let duplicate_query = vec![
            "--fixture".to_owned(),
            "rapid-typing".to_owned(),
            "--query".to_owned(),
            "one".to_owned(),
            "--query=two".to_owned(),
        ];
        assert!(parse_args(&duplicate_query).is_err());

        let duplicate_list = vec!["--list-fixtures".to_owned(), "--list-fixtures".to_owned()];
        assert!(parse_args(&duplicate_list).is_err());
    }
}
