//! The implementation behind CriKey's two executables (spec 28).
//!
//! `crikey` is the console command line and `crikey-launcher` is the graphical
//! entry point; both are thin `main` functions over [`cli_main`] and
//! [`start_launcher`] here, so the launcher has exactly one composition root
//! whichever binary started it. See `src/bin/crikey-launcher.rs` for why two
//! binaries rather than one.

mod activation_commands;
mod calculator;
mod catalog_commands;
mod command_history;
mod command_preferences;
mod config_commands;
mod dev_commands;
mod file_provider;
mod legacy_commands;
mod modern_commands;
mod native_commands;
mod package_commands;
mod plugin_commands;
mod settings;
#[cfg(target_os = "linux")]
mod web_host;

use calculator::Calculator;
use crikey_app::{
    admitted_plugin_roots, ActionEffect, ActionSubmission, AliasTable, App, BatchState,
    DefaultCatalogFetcher, DisabledPlugins, LegacyDriver, LegacyProvider, ModernDriver, ModernProvider,
    NativeDriver, NativeProvider, PipelineConfig, PluginActionRouter, ProviderQueryContext, QueryPipeline,
    RemoteCatalogService, RemoteSource, ResultBatch, SearchService, SelectionHistoryStore, StartupJournal,
    StartupMode, StartupStage,
};
use crikey_benchmarks::{
    run_catalog_benchmark, BenchmarkConfig, BenchmarkReport, PrefixLatency, STRESS_CATALOG_SIZE,
};
use crikey_catalog::{CatalogCache, FileCatalogCache};
use crikey_config::{
    administrator_policy_path, ConfigStore, ConfigurationPublisher, KEY_COALESCE_MS, KEY_MAXIMUM_WAIT_MS,
    KEY_MAX_RESULTS, KEY_RELOAD_INTERVAL_MS,
};
#[cfg(target_os = "linux")]
use crikey_core::WebStorage;
use crikey_core::{Generation, Item, PageWebSurface, PluginId};
use crikey_input_scheduler::{
    ActivationPolicy, DebouncePolicy, PluginPolicy, QueuePolicy, SchedulingProfile,
};
use crikey_legacy_compat::LegacyDeadlines;
use crikey_package_manager::LauncherLock;
use crikey_platform::{DirectoryConvention, PluginKind, StandardDirectories};
use crikey_ui::{
    initial_page_viewport, page_palette, LauncherViewModel, NativeLauncher, NativeLauncherConfig,
    NativeLauncherEvent, PageSurface, UiEffect, ViewModel,
};
use file_provider::{file_provider_policy, FileSearchDriver, FILE_SEARCH_PLUGIN};
use std::cell::RefCell;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[cfg(target_os = "linux")]
use web_host::{WebHostNotice, WebHostSupervisor, WebWake};

const USAGE: &str = "\
crikey - a fast, keyboard-driven application launcher

USAGE:
    crikey                          Start the resident launcher
    crikey <COMMAND> [ARGS]

COMMANDS:
    run                             Start the launcher (use `crikey run --help`)
    settings                        Launcher settings (use `crikey settings --help`)
    plugin                          Plugin management (use `crikey plugin --help`)
    config                          Configuration inspection (use `crikey config --help`)
    catalog                         Remote catalog sources (use `crikey catalog --help`)
    dev                             Developer commands (use `crikey dev --help`)
    package                         Package commands (use `crikey package --help`)
    version                         Print version information
    help                            Print this message

TOP-LEVEL OPTIONS:
    -h, --help                      Print this message
    -V, --version                   Print version information
";
/// Queries the reported percentiles are drawn from, and results each retains.
///
/// Fixed rather than exposed as options: two percentiles only compare when both
/// runs asked the same questions, and these are the numbers the stress-scale
/// test in `crikey-benchmarks` uses, so a report from this command and a report
/// from that test describe one workload rather than two.
const BENCHMARK_QUERIES: usize = 64;
const BENCHMARK_TOP_K: usize = 20;
const APPLICATION_CATALOG_PLUGIN: &str = "builtin.crikey.applications";
fn pipeline_profile(profile: crikey_plugin_model::SchedulingProfile) -> SchedulingProfile {
    match profile {
        crikey_plugin_model::SchedulingProfile::LegacyStrict => SchedulingProfile::LegacyStrict,
        crikey_plugin_model::SchedulingProfile::LegacyOptimized => SchedulingProfile::LegacyOptimized,
        crikey_plugin_model::SchedulingProfile::Modern => SchedulingProfile::Modern,
    }
}

const RUN_USAGE: &str = "\
crikey run - start the launcher

USAGE:
    crikey run
    crikey run --set key=value [--set key=value ...]
    crikey run --help

OPTIONS:
    --set key=value                 Override a setting for this launch only
    -h, --help                      Print this message without starting the launcher
";

const DEV_USAGE: &str = "\
crikey dev - run a developer command

USAGE:
    crikey dev <COMMAND> [ARGS]

COMMANDS:
    run                             Run one modern Python plugin query
    test                            Run modern Python plugin queries
    benchmark                       Measure the catalog path
    trace-query                     Trace deterministic query scheduling
    simulate-typing                Simulate deterministic typing
    test-legacy-compat              Test one legacy package
    inspect-catalog                 Inspect one legacy package catalog
    compatibility-report            Report compatibility data files
    inspect-protocol                Inspect one native plugin protocol
    measure-activation              Measure warm activation

OPTIONS:
    -h, --help                      Print this message
";

const VERSION_USAGE: &str = "\
crikey version - print version information

USAGE:
    crikey version
";

/// The `crikey` console entry point: parse argv, run one subcommand.
pub fn cli_main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    dispatch(&args)
}

/// Where a launch was started from, which decides where a fatal startup failure
/// can be seen.
///
/// The console entry point has a terminal by construction, so stderr is the
/// whole answer there. The desktop entry point has none -- `crikey-launcher` is
/// GUI-subsystem on Windows -- so its stderr is discarded by the operating
/// system and a failed launch would otherwise be a process that vanishes with
/// no explanation anywhere. This distinguishes the two rather than logging
/// unconditionally, so `crikey run` keeps writing exactly what it writes today
/// and leaves no file behind that the operator did not ask for.
#[derive(Clone, Copy, Debug)]
enum LaunchSurface {
    /// `crikey run`, started from a terminal.
    Console,
    /// `crikey-launcher`, started from a shortcut, a tile or a bundle.
    Desktop,
}

/// The `crikey-launcher` entry point: start the launcher with no overrides.
///
/// The same call `crikey run` makes with an empty `--set` list, deliberately
/// routed through [`run_launcher`] rather than reaching past it: the two
/// executables must not be able to drift into two launchers.
pub fn start_launcher() -> ExitCode {
    run_launcher(&[], LaunchSurface::Desktop)
}

fn dispatch(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        // Bare `crikey` starts the launcher rather than printing usage. The
        // first Windows hand test double-clicked `crikey.exe` from Explorer,
        // got the usage text in a console window that closed with the process,
        // and read a program working exactly as designed as a broken one. The
        // usage text is still one `--help` away, which is where someone looking
        // for it types anyway. `Desktop` rather than `Console` for the same
        // reason: a launch started by a double-click has nowhere to show a
        // fatal error, so it also needs the startup log and the dialog.
        None => run_launcher(&[], LaunchSurface::Desktop),
        Some("help") if args.len() == 1 => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("-h" | "--help") if args.len() == 1 => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("version") if args.len() == 1 => {
            println!(
                "crikey {} ({} backend)",
                env!("CARGO_PKG_VERSION"),
                App::platform_backend_name()
            );
            ExitCode::SUCCESS
        }
        Some("version") if args.len() == 2 && matches!(args[1].as_str(), "-h" | "--help") => {
            print!("{VERSION_USAGE}");
            ExitCode::SUCCESS
        }
        Some("run") => {
            if args.len() == 2 && matches!(args[1].as_str(), "-h" | "--help") {
                print!("{RUN_USAGE}");
                ExitCode::SUCCESS
            } else {
                run_launcher(&args[1..], LaunchSurface::Console)
            }
        }
        Some("help") => {
            eprintln!("crikey: `help` takes no arguments\n\n{USAGE}");
            ExitCode::from(64)
        }
        Some("version") => {
            eprintln!("crikey: `version` accepts only `--help`\n\n{VERSION_USAGE}");
            ExitCode::from(64)
        }
        Some("dev") => dev(&args[1..]),
        Some("package") => package_commands::run(&args[1..]),
        Some("plugin") => plugin_commands::run(&args[1..]),
        Some("config") => config_commands::run(&args[1..]),
        Some("settings") => settings::run(&args[1..]),
        Some("catalog") => catalog_commands::run(&args[1..]),
        Some(other) => {
            eprintln!("crikey: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(64) // EX_USAGE
        }
    }
}
fn run_launcher(args: &[String], surface: LaunchSurface) -> ExitCode {
    let mut overrides = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        let value = if argument == "--set" {
            index += 1;
            args.get(index).map(String::as_str)
        } else {
            argument.strip_prefix("--set=")
        };
        let Some(value) = value else {
            eprintln!("crikey: expected `--set key=value`\n\n{RUN_USAGE}");
            return ExitCode::from(64);
        };
        let Some((key, setting)) = value.split_once('=') else {
            eprintln!("crikey: expected `--set key=value`, got `{value}`");
            return ExitCode::from(64);
        };
        if key.is_empty() {
            eprintln!("crikey: `--set` requires a non-empty key");
            return ExitCode::from(64);
        }
        overrides.push((key.to_owned(), setting.to_owned()));
        index += 1;
    }

    match run_native_launcher(&overrides) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let diagnostic = format!("crikey: launcher failed: {message}");
            eprintln!("{diagnostic}");
            // On the desktop path that stderr line has nowhere to go, so the
            // failure is also written somewhere durable and shown to the person
            // who clicked. The argument-parsing refusals above cannot be reached
            // from `crikey-launcher`, which takes no arguments, so this is the
            // only arm that needs the second sink.
            if matches!(surface, LaunchSurface::Desktop) {
                report_desktop_startup_failure(&diagnostic);
            }
            ExitCode::from(70) // EX_SOFTWARE
        }
    }
}

fn run_native_launcher(overrides: &[(String, String)]) -> Result<(), String> {
    // Both of these come before any window, GPU or provider exists, and that
    // order is load-bearing. The launcher lock decides whether this process may
    // run at all, and it must refuse a second launcher on a host with no display
    // just as firmly as on one with a display; reading the disabled set first
    // also means a launch that dies in renderer startup has already said what it
    // made of the operator's configuration.
    // Exactly one launcher per user, held for the life of the process. `crikey
    // plugin install` replaces a plugin directory in place (spec 23.4), and an
    // install racing a live launcher would swap the files out from under a
    // worker mid-query. The guard is bound to a name because dropping it here
    // would release the lock immediately and prove nothing.
    let directories = StandardDirectories::for_process()
        .map_err(|error| format!("cannot resolve the standard directories: {error}"))?;
    let _launcher_lock = LauncherLock::acquire(&directories).map_err(|error| error.to_string())?;
    // The layered configuration (spec 21). Loaded ONCE and retained for the life
    // of the launch: it decides which plugins may load, it bounds the launcher's
    // own result ceiling, and it is the state published to every plugin below.
    // Loading it a second time somewhere else would be two sources of truth
    // separated by a few milliseconds.
    //
    // A configuration that cannot be read is reported and the launch continues on
    // built-in defaults. Refusing the launch would let an unreadable file cost
    // the operator their launcher, and silently disabling everything would be
    let mut configuration = match LauncherConfiguration::load(&directories, overrides) {
        Ok(configuration) => Some(configuration),
        Err(message) => {
            eprintln!("crikey: {message}; this launch uses built-in defaults only");
            None
        }
    };
    // Which plugins the operator has switched off (spec 21.2). Read from the
    // same layered store `crikey plugin disable` writes, and consulted at
    // discovery rather than after loading: the only proof a disabled plugin did
    // not run is that no provider ever started a worker for it.
    let disabled = configuration
        .as_ref()
        .map(|configuration| DisabledPlugins::from_ids(configuration.store.disabled_plugins()))
        .unwrap_or_default();
    // The backend is asked before the window exists, because whether the
    // desktop composites decides whether the window may be a rounded shape at
    // all -- and that is fixed at creation, not per frame.
    let app = App::new();
    let window = NativeLauncherConfig {
        composited: app.desktop_composites(),
        ..NativeLauncherConfig::default()
    };
    // Read before the config is consumed: a page opened later is laid out
    // against the window it will appear in.
    let page_window = (window.width, window.height);
    let launcher = NativeLauncher::new(window).map_err(|error| error.to_string())?;
    let render_handle = launcher.handle();
    let mut search = SearchService::new(app);
    if let Some(configuration) = configuration.as_ref() {
        search.set_aliases(configured_aliases(&configuration.store));
    }
    let catalog_cache_root = catalog_cache_root()?;
    let catalog_cache: Arc<dyn CatalogCache + Send + Sync> =
        Arc::new(FileCatalogCache::new(catalog_cache_root));
    search.set_catalog_cache(Arc::clone(&catalog_cache));
    let loaded_catalog = search
        .load_persisted_catalog(catalog_cache.as_ref())
        .map_err(|error| format!("cannot load persisted catalog cache: {error}"))?;
    if loaded_catalog.skipped > 0 {
        eprintln!(
            "crikey: skipped {} unreadable persisted catalog slice(s)",
            loaded_catalog.skipped
        );
    }
    if let Some(error) = search.catalog_cache_error() {
        eprintln!("crikey: catalog cache error during startup load: {error}");
    }

    let hotkey_wake = render_handle.clone();
    let mut hotkeys = search.command_hotkeys(move || {
        let _ = hotkey_wake.request_action_completion();
    });
    let mut hotkey_inputs = None;
    let mut hotkey_authoritative = true;

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
    if let Some(error) = search.catalog_cache_error() {
        eprintln!("crikey: application catalog cache write failed: {error}");
    }
    search
        .complete_stage(StartupStage::PersistedCatalog)
        .map_err(|error| error.to_string())?;
    // Remote catalog sources (spec 2.2, ADR-0016). Built after the persisted
    // slices are in, because the retained slice of a source that is currently
    // unreachable is exactly what keeps serving while the refresh below fails.
    // No source configured means an empty service: no thread, no socket, no
    // behaviour change of any kind.
    let mut remote_catalog = remote_catalog_service(&directories, configuration.as_ref());
    // Monotonic, and independent of the wall clock a user can move: refresh
    // intervals are durations, not appointments.
    let remote_clock = Instant::now();
    // Ranking history is restored before the first query can be accepted, so
    // the very first result list a user sees already reflects what they picked
    // last time. Restoring after `AcceptQueries` would leave a window in which
    // a query is answered from an empty history and then silently reranked.
    let selection_history = match selection_history_path() {
        Some(path) => Some(SelectionHistoryStore::new(path)),
        None => {
            eprintln!(
                "crikey: no per-user state directory (set XDG_STATE_HOME or HOME); \
                 ranking history is not kept across launches"
            );
            None
        }
    };
    if let Some(store) = selection_history.as_ref() {
        search.restore_selection_history(store.load());
    }
    // The recency term is scored against this clock, so it has to hold a real
    // wall-clock time before the first query rather than the zero a fresh
    // service starts at: at zero, every persisted selection looks like it
    // happened decades in the future and recency contributes nothing.
    let mut history_clock = HistoryClock::default();
    search.set_history_time(history_clock.advance());
    search
        .complete_stage(StartupStage::AcceptQueries)
        .map_err(|error| error.to_string())?;
    // SearchService remains the synchronous matcher and ranker. Its actual
    // result items still cross the M2 intake and presentation boundary before
    // its richer ranked/highlighted rows become visible.
    let pipeline_bounds = pipeline_config(configuration.as_ref());
    let mut query_pipeline = QueryPipeline::new(pipeline_bounds);
    query_pipeline
        .register_plugin(owner.clone(), application_provider_policy())
        .map_err(|error| {
            format!("cannot register the application provider with the query pipeline: {error}")
        })?;

    // Legacy plugins join the live query path here, not only the `crikey dev`
    // commands. Without this the Legacy Compatibility Layer would never load a
    // real package or serve a suggestion through the pipeline that `crikey run`
    // drives (spec 14.5; roadmap M3). No modern worker is required at this
    // milestone, so the required-worker milestone is trivially complete and is
    // acknowledged only to reach the legacy milestone in specification order.
    search
        .complete_stage(StartupStage::RequiredWorkers)
        .map_err(|error| error.to_string())?;

    // Startup recovery (spec 24.2). The journal is opened and the attempt is
    // committed BEFORE any third-party runtime loads: a launch that dies while
    // loading a plugin must leave that attempt on disk, and the mode it was
    // admitted under has to gate every provider below.
    let mut journal = match startup_journal_path() {
        Some(path) => Some(StartupJournal::load(&path)),
        None => {
            eprintln!(
                "crikey: no per-user state directory (set XDG_STATE_HOME or HOME); \
                 startup recovery is disabled for this launch"
            );
            None
        }
    };
    if let Some(blamed) = journal
        .as_ref()
        .map(StartupJournal::active_during_abnormal_shutdown)
        .filter(|blamed| !blamed.is_empty())
    {
        let names = blamed
            .iter()
            .map(|id| id.0.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("crikey: previous shutdown was abnormal; plugins then active: {names}");
    }
    let startup_mode = match journal.as_mut() {
        Some(journal) => {
            let mode = journal.begin_startup(std::slice::from_ref(&owner));
            commit_startup_journal(journal);
            mode
        }
        None => StartupMode::Normal,
    };
    if let StartupMode::SafeMode { consecutive_failures } = startup_mode {
        eprintln!(
            "crikey: safe mode after {consecutive_failures} consecutive failed startups; \
             third-party plugins are disabled (spec 24.2)"
        );
    }
    // The record from here on is owned by a ledger shared with the renderer
    // callback: the composition root refreshes the active plugin set as each
    // provider comes up, and the callback is what proves the renderer started.
    let ledger = StartupLedger::new(journal);
    // Every third-party runtime is gated, not just the native one: a safe-mode
    // boot that still spawns a legacy or modern interpreter would defeat 24.2.
    let mut active_plugins = vec![owner.clone()];
    let mut legacy_pipeline = QueryPipeline::new(pipeline_bounds);
    let legacy_cache_root = legacy_cache_root()?;
    let mut legacy_provider = LegacyProvider::load(
        &mut legacy_pipeline,
        &admitted_plugin_roots(
            &startup_mode,
            &discovery_roots(PluginKind::Legacy, legacy_package_roots(), &directories),
        ),
        legacy_cache_root,
        crikey_app::LegacyDirectories {
            user_config: Some(directories.config_dir().to_path_buf()),
            installed_packages: Some(directories.plugin_dir(PluginKind::Legacy)),
        },
        LegacyDeadlines::default(),
        &disabled,
    );
    // Apply persisted profile overrides after discovery. Provider loading owns
    // registration, so this is the first point at which every runtime's
    // discovered plugin id is available to the production pipeline.
    if let Some(configuration) = configuration.as_ref() {
        for plugin in legacy_provider.plugins() {
            if let Some(profile) = configuration.store.scheduling_profile(plugin) {
                let _ = legacy_pipeline.set_scheduling_profile(plugin, pipeline_profile(profile));
            }
        }
    }
    // A legacy plugin publishes its searchable rows from `on_catalog`, so a
    // launcher that never asks for one serves nothing from it but live
    // suggestions (spec 14.8). Admission happens here; the callback itself
    // runs on the supervisor thread the driver spawns below.
    // The plugin list is copied out before the loop because
    // `request_catalog_build` needs `&mut legacy_provider`: iterating the
    // borrowed slice would hold the provider immutably for the whole loop.
    let legacy_plugins: Vec<PluginId> = legacy_provider.plugins().to_vec();
    for plugin in &legacy_plugins {
        if let Err(error) = legacy_provider.request_catalog_build(plugin, 1, crikey_core::Generation::ZERO) {
            eprintln!("crikey: legacy catalog request refused for {}: {error}", plugin.0);
        }
    }
    for entry in legacy_provider.unavailable() {
        eprintln!(
            "crikey: legacy plugin unavailable ({}): {}",
            entry.package, entry.reason
        );
    }
    active_plugins.extend_from_slice(legacy_provider.plugins());
    // Committed before the supervisor is spawned and before anything below can
    // fail: a launch that dies between two providers must leave behind the
    // plugins that were active at that moment, not the set it would have had
    // if it had finished (spec 24.2).
    ledger.borrow_mut().record_active(&active_plugins);
    // Truthful staging (spec 25.6): the milestone is acknowledged only now that
    // legacy loading has actually completed.
    search
        .complete_stage(StartupStage::LegacyPlugins)
        .map_err(|error| error.to_string())?;
    // Workers only wake the host. The UI thread owns the complete canonical
    // merge, so an individual provider cannot publish a partial frame over it.
    let legacy_publish_handle = render_handle.clone();
    let legacy_driver = LegacyDriver::spawn(legacy_provider, legacy_pipeline, move |_| {
        let _ = legacy_publish_handle.request_provider_answer();
    });

    // Modern python plugins join the same live query path, driven off the UI
    // thread by their own supervisor (spec 15.6; acceptance 31.10). Discovery
    // scans `CRIKEY_MODERN_PLUGIN_ROOTS`; an unset variable loads nothing, so
    // `crikey run` behaves exactly as before on a host with no modern plugins.
    // A crashing interpreter degrades to a recorded diagnostic and never aborts
    // the process (contract §8), so no load failure here is fatal to the launch.
    let mut modern_pipeline = QueryPipeline::new(pipeline_bounds);
    let cache_root = modern_cache_root()?;
    let mut modern_provider = ModernProvider::load(
        &mut modern_pipeline,
        &admitted_plugin_roots(
            &startup_mode,
            &discovery_roots(PluginKind::Modern, modern_plugin_roots(), &directories),
        ),
        modern_index_root(),
        cache_root,
        &disabled,
    );
    if let Some(configuration) = configuration.as_ref() {
        for plugin in modern_provider.plugins() {
            if let Some(profile) = configuration.store.scheduling_profile(plugin) {
                let _ = modern_pipeline.set_scheduling_profile(plugin, pipeline_profile(profile));
            }
        }
    }
    // Before any build is requested: a plugin that refuses catalog persistence
    // must have last run's slice withdrawn even if its build never completes.
    for (plugin, persist) in modern_provider.catalog_declarations() {
        search.set_catalog_persistence(&plugin, persist);
    }
    let modern_plugins = modern_provider.plugins().to_vec();
    for plugin in modern_plugins {
        if let Err(error) = modern_provider.request_catalog_build(&plugin, 1, crikey_core::Generation::ZERO) {
            eprintln!("crikey: modern catalog request refused for {}: {error}", plugin.0);
        }
    }
    for entry in modern_provider.unavailable() {
        eprintln!(
            "crikey: modern plugin unavailable ({}): {}",
            entry.package, entry.reason
        );
    }
    active_plugins.extend_from_slice(modern_provider.plugins());
    ledger.borrow_mut().record_active(&active_plugins);
    // Like legacy, modern outcomes are folded only by the host.
    let modern_publish_handle = render_handle.clone();
    let modern_driver = ModernDriver::spawn(modern_provider, modern_pipeline, move |_| {
        let _ = modern_publish_handle.request_provider_answer();
    });

    // Native plugins use the same asynchronous query boundary as the legacy
    // and modern providers. Discovery is intentionally empty unless the
    // operator names native package roots (spec 16.1, 16.6).
    let mut native_pipeline = QueryPipeline::new(pipeline_bounds);
    let mut native_provider = NativeProvider::load(
        &mut native_pipeline,
        &admitted_plugin_roots(
            &startup_mode,
            &discovery_roots(PluginKind::Native, native_plugin_roots(), &directories),
        ),
        &disabled,
    );
    if let Some(configuration) = configuration.as_ref() {
        for plugin in native_provider.plugins() {
            if let Some(profile) = configuration.store.scheduling_profile(plugin) {
                let _ = native_pipeline.set_scheduling_profile(plugin, pipeline_profile(profile));
            }
        }
    }
    // Before any build is requested: a plugin that refuses catalog persistence
    // must have last run's slice withdrawn even if its build never completes.
    for (plugin, persist) in native_provider.catalog_declarations() {
        search.set_catalog_persistence(&plugin, persist);
    }
    let native_plugins = native_provider.plugins().to_vec();
    for plugin in native_plugins {
        if let Err(error) = native_provider.request_catalog_build(&plugin, 1, crikey_core::Generation::ZERO) {
            eprintln!("crikey: native catalog request refused for {}: {error}", plugin.0);
        }
    }
    for entry in native_provider.unavailable() {
        eprintln!(
            "crikey: native plugin unavailable ({}): {}",
            entry.package, entry.reason
        );
    }
    active_plugins.extend_from_slice(native_provider.plugins());
    ledger.borrow_mut().record_active(&active_plugins);
    let native_publish_handle = render_handle.clone();
    let native_driver = NativeDriver::spawn(
        native_provider,
        native_pipeline,
        Box::new(move |_| {
            let _ = native_publish_handle.request_provider_answer();
        }),
    );
    // The host's own file search joins the live query path here. It gets its own
    // pipeline because its policy is its own: it debounces where the
    // application provider fires on the leading edge, and one pipeline cannot
    // hold two answers to when a query should be dispatched.
    let file_owner = PluginId(FILE_SEARCH_PLUGIN.to_owned());
    let mut file_pipeline = QueryPipeline::new(pipeline_bounds);
    file_pipeline
        .register_plugin(file_owner.clone(), file_provider_policy())
        .map_err(|error| format!("cannot register the file provider with the query pipeline: {error}"))?;
    // The worker cannot build this provider's frame: its items are ranked
    // against the catalog by a service that lives on the UI thread. So the
    // driver only says "there is an answer", and the merge happens on the next
    // turn of the event loop, which this wake is what causes.
    let file_wake_handle = render_handle.clone();
    let file_driver = FileSearchDriver::spawn(
        file_owner.clone(),
        file_pipeline,
        // Built on the driver's own worker thread rather than here: a platform
        // backend need not be `Send`, and this one never has to be.
        Box::new(|| Box::new(App::new())),
        move || {
            let _ = file_wake_handle.request_provider_answer();
        },
    );
    // Page frames and action completions are produced on the native
    // supervisor's threads, so they need the same wake the file driver uses:
    // the loop is asleep between keystrokes, and a page that only repainted
    // when the user pressed an unrelated key would look frozen.
    let page_wake_handle = render_handle.clone();
    native_driver.set_wake(move || {
        let _ = page_wake_handle.request_provider_answer();
    });
    // Action terminals have process lifetime, not presentation lifetime. Drain
    // them while hidden too, without reactivating a retired launcher session.
    let action_wake_handle = render_handle.clone();
    native_driver.set_action_wake(move || {
        let _ = action_wake_handle.request_action_completion();
    });
    let action_wake_handle = render_handle.clone();
    modern_driver.set_action_wake(move || {
        let _ = action_wake_handle.request_action_completion();
    });
    let action_wake_handle = render_handle.clone();
    legacy_driver.set_action_wake(move || {
        let _ = action_wake_handle.request_action_completion();
    });
    // Plugin-owned actions use the same exact-owner endpoints and budget
    // handles retained by the provider drivers. Registering this router before
    // the event loop makes `crikey run` execute selected legacy/modern/native
    // actions instead of falling through to host launch handling.
    let mut action_router = PluginActionRouter::default();
    action_router
        .register(legacy_driver.plugins(), legacy_driver.action_executor())
        .map_err(|error| format!("cannot register legacy action runtime: {error}"))?;
    action_router
        .register_with_permissions(modern_driver.permissions(), modern_driver.action_executor())
        .map_err(|error| format!("cannot register modern action runtime: {error}"))?;
    action_router
        .register_with_permissions(native_driver.permissions(), native_driver.action_executor())
        .map_err(|error| format!("cannot register native action runtime: {error}"))?;
    // The discovered-application catalog is the host's own, published under a
    // builtin owner with no plugin runtime. It still has to appear in the
    // grant map: the launch gate refuses an owner it does not know, and
    // without this line `crikey run` would refuse to launch the applications
    // it discovered itself.
    action_router
        .register_host_catalog(PluginId(APPLICATION_CATALOG_PLUGIN.to_owned()))
        .map_err(|error| format!("cannot register the application catalog grants: {error}"))?;
    // The file provider is the host's own too, and needs the same entry for the
    // same reason: without it every file row would render and then refuse to
    // open, because the host-mediated gate refuses an owner it does not know.
    action_router
        .register_host_catalog(file_owner.clone())
        .map_err(|error| format!("cannot register the file provider grants: {error}"))?;
    // One `Arc`, shared. The legacy driver needs the same router the search
    // service uses: a host-mediated action a legacy plugin asks for is gated by
    // exactly the grants registered above, and a second router would be a
    // second answer to the same question.
    let action_router = Arc::new(action_router);
    legacy_driver.set_plugin_action_router(Arc::clone(&action_router));
    search.set_plugin_action_router(action_router);
    // The first publication (spec 21.4). Flushed rather than coalesced: startup is
    // not a burst of edits, and a plugin whose `on_configuration` decides where to
    // look for its data must be told before it serves its first query.
    if let Some(configuration) = configuration.as_mut() {
        configuration.seed(Instant::now());
        if let Some(state) = configuration.publisher.flush() {
            publish_configuration(&modern_driver, &native_driver, state.plugins().clone());
        }
    }
    let query_clock = Instant::now();

    let activation_handle = render_handle.clone();
    activation_handle
        .request_activation()
        .map_err(|error| error.to_string())?;
    // `request_activation` only QUEUES an event: the window and GPU are built
    // later, in the event loop's `resumed`, and can still fail terminally
    // there. Readiness is therefore recorded by the callback below, on the
    // first event the loop actually delivers - see `ready_on_first_event`.
    let mut view_model = LauncherViewModel::new();
    let history_expiry = Duration::from_secs(24 * 60 * 60);
    let mut query_history = per_user_state_dir().map_or_else(
        || crikey_app::QueryHistoryStore::session(history_expiry),
        |dir| crikey_app::QueryHistoryStore::new(dir.join("query-history.json"), history_expiry),
    );
    settings::apply_navigation_preferences(
        configuration.as_ref().map(|configuration| &configuration.store),
        &mut view_model,
        &mut query_history,
    );
    // The settings panel has content before the user opens it, because the one
    // occasion it opens by itself is a hotkey that could not be bound — and a
    // panel that came up empty on exactly that occasion would be no better than
    // the silence it replaces.
    view_model.set_settings(settings::rows(
        configuration.as_ref().map(|configuration| &configuration.store),
    ));
    // And the hint line's visibility, from the same store, before the first
    // frame: a launcher that showed the hints once and hid them on the next
    // activation would look like the setting had not taken.
    view_model.set_show_hints(settings::configured_show_hints(
        configuration.as_ref().map(|configuration| &configuration.store),
    ));
    // And the window's shape, from that same store, for the same reason: the
    // corners are drawn once per frame, so a launcher that came up round and
    // squared off on the next activation would look like the setting had not
    // taken.
    view_model.set_rounded_corners(settings::configured_rounded_corners(
        configuration.as_ref().map(|configuration| &configuration.store),
    ));
    view_model.set_preview_ratio(settings::configured_preview_split(
        configuration.as_ref().map(|configuration| &configuration.store),
    ));
    let preview_wake = render_handle.clone();
    let mut previews = crikey_app::PreviewService::new(&native_driver, &modern_driver, move || {
        let _ = preview_wake.request_provider_answer();
    });
    let preference_wake = render_handle.clone();
    let preference_worker = command_preferences::PreferenceWorker::spawn(
        search.command_preferences_store(),
        configuration
            .as_ref()
            .map(|configuration| configuration.store.clone()),
        directories.clone(),
        overrides.to_vec(),
        move || {
            let _ = preference_wake.request_action_completion();
        },
    )?;
    let history_wake = render_handle.clone();
    let history_writer = command_history::HistoryWorker::spawn(selection_history.clone(), move || {
        let _ = history_wake.request_action_completion();
    })?;
    // The built-in calculator, holding this session's clipboard if it has one.
    // Taken here, before the event loop, and then owned by the closure for the
    // rest of the process: on X11 a copied value is served by the client that
    // owns the selection, so the clipboard has to outlive every copy made
    // through it, and this is the one place in the launcher with that lifetime.
    // Still an `Option`: a session with no display server has no clipboard, and
    // the calculator renders its row and explains itself instead of pretending
    // a copy happened.
    let mut calculator = Calculator::new(search.clipboard());
    #[cfg(target_os = "linux")]
    let mut web_page = {
        // The manager thread and WPE process are both lazy: a launcher that
        // never opens an admitted web page pays no process or thread startup
        // cost.
        let web_wake_handle = render_handle.clone();
        let web_wake: WebWake = Arc::new(move || {
            let _ = web_wake_handle.request_provider_answer();
        });
        WebPageRuntime::new(
            directories.data_dir().join("web-storage"),
            web_wake,
            initial_page_viewport(page_window.0, page_window.1),
        )
    };
    #[cfg(not(target_os = "linux"))]
    let mut web_page = WebPageRuntime::new();
    let mut retained = RetainedRows::default();
    let mut action_origins = std::collections::BTreeMap::new();
    let mut typed_revision = 0u64;
    let mut typed_requests = std::collections::BTreeMap::new();
    let mut command_projection_dirty = true;
    let mut active_root_query = String::new();
    let mut pending_command_learning: std::collections::BTreeMap<
        crikey_app::ActionRequestId,
        crikey_app::CommandLearning,
    > = std::collections::BTreeMap::new();
    let mut pending_ordinary_learning = std::collections::BTreeMap::new();
    let mut pending_typed_items = std::collections::BTreeMap::new();
    let mut ranking_reset_pending = false;
    let mut quit_after_history_reset = false;
    let mut deferred_learning = Vec::new();
    let mut ordinary_catalog_state = crikey_app::OrdinaryProviderState::Pending;
    let mut last_ordinary_evidence = Vec::new();
    let mut command_projection_revision = 0;
    let mut command_management_revision = None;
    let mut command_failure_notice = None;
    let mut configuration_retry = Instant::now();
    let mut hotkey_intent = 0u64;
    let mut hotkey_presentations = std::collections::BTreeMap::new();
    let mut prepared_activation = false;
    let mut hotkey_report = crikey_app::ReconcileReport::default();
    retained.max_results = Some(pipeline_bounds.limits.max_items_per_query);
    // Per-plugin refusal totals already reported, so a growing counter is
    // announced once per increase rather than on every turn of the loop.
    let mut reported_refusals: std::collections::BTreeMap<PluginId, crikey_app::ConcurrencyRefusals> =
        std::collections::BTreeMap::new();
    let outcome = launcher
        .run(ready_on_first_event(Rc::clone(&ledger), move |event| {
            if matches!(&event, NativeLauncherEvent::Command { .. })
                || matches!(&event, NativeLauncherEvent::Activated) && !prepared_activation
            {
                hotkey_intent = hotkey_intent.checked_add(1).expect("shortcut intent exhausted");
            }
            let hotkey_receipts = hotkeys.drain_events();
            if !hotkey_receipts.is_empty() {
                hotkey_intent = hotkey_intent.checked_add(1).expect("shortcut intent exhausted");
            }
            let mut page_failure_status = None;
            while let Some(completion) = preference_worker.take() {
                hotkey_authoritative = matches!(
                    completion.state,
                    command_preferences::CompletionState::Authoritative
                );
                if let Some(outcome) = completion.command_edit {
                    view_model.command_edit_completed(outcome);
                }
                if let Some(outcome) = completion.setting_edit {
                    view_model.setting_edit_completed(outcome);
                }
                let current = configuration.as_ref().is_none_or(|configuration| {
                    configuration.store.revision() == completion.based_on
                        || completion
                            .config
                            .as_ref()
                            .is_some_and(|store| store.revision() == configuration.store.revision())
                });
                if !current {
                    let _ = preference_worker.submit(command_preferences::Request::Poll);
                    view_model.set_navigation_notice(
                        "Configuration changed again; refreshing the effective saved state".into(),
                    );
                    continue;
                }
                if let Some(store) = completion.config {
                    if let Some(configuration) = configuration.as_mut() {
                        configuration.store = store;
                    } else {
                        configuration = Some(LauncherConfiguration::from_store(store));
                    }
                    let configuration = configuration
                        .as_mut()
                        .expect("a completed load publishes a configuration");
                    configuration.reload_interval = millis(&configuration.store, KEY_RELOAD_INTERVAL_MS);
                    configuration.seed(Instant::now());
                    configuration.reloaded = true;
                    command_projection_dirty = true;
                }
                if let Some(snapshot) = completion.preferences {
                    search.publish_command_preferences(snapshot);
                    command_projection_dirty = true;
                }
                if let Some(report) = completion.report {
                    view_model.set_navigation_notice(report);
                }
            }
            while let Some(completion) = history_writer.take() {
                match completion.result {
                    Ok(Some(snapshot)) => {
                        search.restore_selection_history(snapshot);
                        command_projection_dirty = true;
                        view_model.set_navigation_notice(
                            "Learned item ranking reset; preferences and command recents retained".into(),
                        );
                    }
                    Ok(None) => {}
                    Err(message) => view_model.set_navigation_notice(message),
                }
                if completion.reset {
                    ranking_reset_pending = false;
                    search.set_history_reset_pending(false);
                    let had_deferred = !deferred_learning.is_empty();
                    for learning in deferred_learning.drain(..) {
                        if let Err(message) = command_history::Learning::publish(learning, &mut search) {
                            view_model.set_navigation_notice(message.into());
                        }
                    }
                    if had_deferred {
                        command_projection_dirty = true;
                        if let Err(message) = history_writer.save(search.selection_history_snapshot()) {
                            view_model.set_navigation_notice(message);
                        }
                    }
                }
            }
            // Live configuration (spec 21.4). Checked once per turn of the loop:
            // the reloader re-stats its files at most every
            // `launcher.configuration-reload-interval-ms`, and the publisher
            // hands over a state only once the edits have settled. The honest
            // bound is that an idle launcher with no events pending notices a
            // change on its next event — which is before it answers the query
            // the user is about to type, because the provider drivers take a
            // configuration publication ahead of a queued query.
            if configuration.is_none()
                && configuration_retry.elapsed() >= Duration::from_millis(500)
                && preference_worker
                    .submit(command_preferences::Request::Poll)
                    .is_ok()
            {
                configuration_retry = Instant::now();
            }
            if let Some(configuration) = configuration.as_mut() {
                if let Some(state) = configuration.poll(Instant::now(), &preference_worker) {
                    publish_configuration(&modern_driver, &native_driver, state);
                }
                if configuration.take_reloaded() {
                    command_projection_dirty |= retained.configure(&configuration.store);
                    view_model.set_settings(settings::rows(Some(&configuration.store)));
                    view_model.set_show_hints(settings::configured_show_hints(Some(&configuration.store)));
                    view_model.set_rounded_corners(settings::configured_rounded_corners(Some(
                        &configuration.store,
                    )));
                    view_model
                        .set_preview_ratio(settings::configured_preview_split(Some(&configuration.store)));
                    settings::apply_navigation_preferences(
                        Some(&configuration.store),
                        &mut view_model,
                        &mut query_history,
                    );
                    // Aliases are host configuration, so a hand edit takes
                    // effect on the next query rather than the next restart.
                    search.set_aliases(configured_aliases(&configuration.store));
                }
            }
            // Remote catalog sources (spec 2.2, ADR-0016). Both calls return
            // immediately: `poll` starts a fetch on a thread of its own and
            // `apply` admits only documents that already finished, through the
            // same publication edge the provider catalog results below use.
            // Nothing here is reachable from the query path, and a launcher
            // with no source configured skips it entirely (README invariant 2).
            if !remote_catalog.is_idle() {
                let now_ms = u64::try_from(remote_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
                remote_catalog.poll(now_ms);
                for report in remote_catalog.apply(&mut search, now_ms) {
                    eprintln!("crikey: {report}");
                }
            }
            // Fold any legacy rows the supervisor produced since the last turn
            for completion in search.poll_action_completions() {
                if let Some((revision, draft_revision, view, generation)) =
                    typed_requests.remove(&completion.request_id)
                {
                    let outcome = match completion.outcome {
                        Ok(ActionEffect::Typed(outcome)) => outcome,
                        _ => crikey_core::structured_arguments::SubmissionOutcome::Failed(
                            crikey_core::structured_arguments::SubmissionFailureCode::Failed,
                        ),
                    };
                    let command_learning = pending_command_learning.remove(&completion.request_id);
                    let item_learning = pending_typed_items.remove(&completion.request_id);
                    if matches!(
                        &outcome,
                        crikey_core::structured_arguments::SubmissionOutcome::Completed
                    ) {
                        let time = history_clock.advance();
                        let learning = command_learning
                            .map(|learning| command_history::Learning::Command(learning, time))
                            .or_else(|| {
                                item_learning.map(|item| command_history::Learning::ItemSelection(item, time))
                            });
                        if let Some(learning) = learning {
                            command_projection_dirty = true;
                            if let Err(message) = publish_learning(
                                learning,
                                &mut search,
                                &history_writer,
                                ranking_reset_pending,
                                &mut deferred_learning,
                            ) {
                                view_model.set_navigation_notice(message);
                            }
                        }
                    }
                    if view_model.accepts_typed_draft(revision, draft_revision, view, generation) {
                        view_model.inline_arguments_completed(revision, draft_revision, outcome);
                    }
                    continue;
                }
                let command_learning = pending_command_learning.remove(&completion.request_id);
                let ordinary_learning = pending_ordinary_learning.remove(&completion.request_id);
                if matches!(
                    &completion.outcome,
                    Ok(ActionEffect::Completed
                        | ActionEffect::ShowPage { .. }
                        | ActionEffect::Typed(
                            crikey_core::structured_arguments::SubmissionOutcome::Completed
                        ))
                ) {
                    let time = history_clock.advance();
                    let learning = command_learning
                        .map(|evidence| command_history::Learning::Command(evidence, time))
                        .or_else(|| {
                            ordinary_learning
                                .map(|evidence| command_history::Learning::Ordinary(evidence, time))
                        });
                    if let Some(learning) = learning {
                        command_projection_dirty = true;
                        if let Err(message) = publish_learning(
                            learning,
                            &mut search,
                            &history_writer,
                            ranking_reset_pending,
                            &mut deferred_learning,
                        ) {
                            view_model.set_navigation_notice(message);
                        }
                    }
                }
                let origin = action_origins.remove(&completion.request_id).flatten();
                let may_present = origin.is_some_and(|(view, session)| {
                    render_handle.is_visible_session(session)
                        && view_model.view().is_some_and(|(current, _)| current == view)
                });
                let deliberate = hotkey_presentations
                    .remove(&completion.request_id)
                    .is_some_and(|intent| intent == hotkey_intent);
                let mut may_present = may_present;
                if deliberate && matches!(&completion.outcome, Ok(ActionEffect::ShowPage { .. })) {
                    if render_handle.is_visible() {
                        may_present = true;
                    } else {
                        activate_launcher_session(&mut search, &mut view_model, &mut web_page);
                        if render_handle.request_activation().is_ok() {
                            prepared_activation = true;
                            may_present = true;
                        }
                    }
                }
                if !may_present {
                    // Draining still releases endpoint admission/budgets. A
                    // completion from a hidden or replaced view cannot open a
                    // page or attach status to the next launcher session.
                    if let Err(error) = completion.outcome {
                        eprintln!(
                            "crikey: action failed ({} / {}): {error}",
                            completion.plugin.0,
                            completion
                                .action_id
                                .as_ref()
                                .map_or("default callback", |action| action.0.as_str())
                        );
                    }
                    continue;
                }
                let message = match completion.outcome {
                    Ok(ActionEffect::Typed(_)) => "Typed command finished".into(),
                    Ok(ActionEffect::Completed) => format!(
                        "Action completed: {} / {}",
                        completion.plugin.0,
                        completion
                            .action_id
                            .as_ref()
                            .map_or("default callback", |action| action.0.as_str())
                    ),
                    // The action did not finish, it took over the window. The
                    // launcher stays open and the plugin starts drawing; a
                    // dismissal here would close the surface it just asked
                    // for.
                    Ok(ActionEffect::ShowPage { page_id }) => {
                        match open_plugin_page(
                            &mut search,
                            &mut view_model,
                            &mut web_page,
                            &completion.plugin,
                            &page_id,
                            page_window,
                        ) {
                            Ok(()) => continue,
                            Err(error) => {
                                format!("Page failed ({} / {page_id}): {error}", completion.plugin.0)
                            }
                        }
                    }
                    Err(error) => format!(
                        "Action failed ({} / {}): {error}",
                        completion.plugin.0,
                        completion
                            .action_id
                            .as_ref()
                            .map_or("default callback", |action| action.0.as_str())
                    ),
                };
                report_status(&mut view_model, message);
            }
            // A page answers on its own thread like any other provider, so its
            // finished frames are folded in here rather than on the keystroke
            // that caused them. `poll_page` yields at most one update a turn.
            if let Some(update) = search.poll_page() {
                if update.closed {
                    web_page.close("plugin page closed");
                    if let Some(reason) = update.failure {
                        if let Some(page) = view_model.page() {
                            let route = crikey_ui::NavigationRoute::CustomPage {
                                plugin: page.plugin.clone(),
                                page_id: page.page_id.clone(),
                                title: page.plugin_name.clone(),
                            };
                            view_model.page_reopen_failed(route, reason);
                        } else {
                            view_model.set_navigation_notice(format!("Page unavailable: {reason}"));
                        }
                    } else {
                        view_model.close_page();
                    }
                } else {
                    let owner = search.page_owner().map(|(plugin, _)| plugin);
                    let frame = Arc::new(update.frame);
                    let declaration = frame.web.clone();
                    if view_model.update_page(frame, false) {
                        match (owner, declaration) {
                            (Some(plugin), Some(declaration)) => {
                                if let Err(error) =
                                    web_page.declare(&search, &mut view_model, &plugin, &declaration)
                                {
                                    page_failure_status = Some(close_failed_web_page(
                                        &mut web_page,
                                        &mut search,
                                        &mut view_model,
                                        error,
                                    ));
                                }
                            }
                            (Some(_), None) => {
                                web_page.close("page switched to a display list");
                            }
                            (None, _) => {
                                page_failure_status = Some(close_failed_web_page(
                                    &mut web_page,
                                    &mut search,
                                    &mut view_model,
                                    "the page owner disappeared before its frame was admitted".to_owned(),
                                ));
                            }
                        }
                    }
                }
            }
            #[cfg(target_os = "linux")]
            {
                let mut web_failure = None;
                for notice in web_page.poll() {
                    match notice {
                        WebHostNotice::Frame(frame) => {
                            view_model.update_web_frame(frame);
                        }
                        WebHostNotice::Caret {
                            surface_id,
                            x,
                            y,
                            width,
                            height,
                        } => {
                            view_model.update_web_caret_area(surface_id, x, y, width, height);
                        }
                        WebHostNotice::Fatal { surface_id, message } => {
                            view_model.clear_web_frame(surface_id);
                            web_failure = Some(message);
                        }
                    }
                }
                if let Some(message) = web_failure {
                    page_failure_status = Some(close_failed_web_page(
                        &mut web_page,
                        &mut search,
                        &mut view_model,
                        message,
                    ));
                }
            }
            // into the retained view model, so a later navigation keystroke
            // keeps them. `publish` refuses a superseded or retired generation,
            // so a late answer can never land under a newer one.
            if let Some(outcome) = legacy_driver.take_outcome() {
                retained.absorb(
                    outcome.generation,
                    RowSource::Legacy,
                    &outcome.rows,
                    outcome.pending_plugins,
                );
                publish_projected_rows(&search, &retained, &mut view_model, outcome.generation);
                command_projection_dirty = true;
            }
            // Each supervisor returns only its own suggestions. The host keeps
            // the canonical ranked answer and all other providers' rows.
            if let Some(outcome) = modern_driver.take_outcome() {
                retained.absorb(
                    outcome.generation,
                    RowSource::Modern,
                    &outcome.rows,
                    outcome.pending_plugins,
                );
                publish_projected_rows(&search, &retained, &mut view_model, outcome.generation);
                command_projection_dirty = true;
            }
            // Native rows are folded independently, preserving both existing
            // provider groups and stale-generation rejection.
            if let Some(outcome) = native_driver.take_outcome() {
                retained.absorb(
                    outcome.generation,
                    RowSource::Native,
                    &outcome.rows,
                    outcome.pending_plugins,
                );
                publish_projected_rows(&search, &retained, &mut view_model, outcome.generation);
                command_projection_dirty = true;
            }
            // File items are ranked in, not appended: the search ran on the
            // driver's worker thread, so its answer arrives on some later turn
            // of this loop — under the generation that asked for it or not at
            // all — and is scored against the catalog here before anything is
            // published. The clock is what lets the driver answer for a worker
            // that never will: it accepted the generation this source was
            // marked pending for, so it, and not this loop, owes the outcome
            // that settles it.
            let file_now = u64::try_from(query_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
            if let Some(outcome) = file_driver.take_outcome(file_now) {
                let generation = outcome.generation;
                absorb_file_items(&mut search, &mut retained, &file_owner, outcome);
                publish_projected_rows(&search, &retained, &mut view_model, generation);
                command_projection_dirty = true;
            }
            // Catalog outcomes use the same SearchService instance/owner
            // publication edge as persisted slices. Obsolete and failed
            // results are observable but can never replace live state.
            for result in legacy_driver.take_catalog_results() {
                match result {
                    crikey_app::CatalogBuildResult::Complete(build) => {
                        if let Err(error) = build.publish(&mut search) {
                            eprintln!("crikey: legacy catalog publication refused: {error}");
                        }
                    }
                    crikey_app::CatalogBuildResult::Failed { reason, .. } => {
                        eprintln!("crikey: legacy catalog build failed: {reason}");
                    }
                    crikey_app::CatalogBuildResult::Obsolete(_) => {}
                }
            }
            for result in modern_driver.take_catalog_results() {
                match result {
                    crikey_app::CatalogBuildResult::Complete(build) => {
                        if let Err(error) = build.publish(&mut search) {
                            eprintln!("crikey: modern catalog publication refused: {error}");
                        }
                    }
                    crikey_app::CatalogBuildResult::Failed { reason, .. } => {
                        eprintln!("crikey: modern catalog build failed: {reason}");
                    }
                    crikey_app::CatalogBuildResult::Obsolete(_) => {}
                }
            }
            for result in native_driver.take_catalog_results() {
                match result {
                    crikey_app::CatalogBuildResult::Complete(build) => {
                        if let Err(error) = build.publish(&mut search) {
                            eprintln!("crikey: native catalog publication refused: {error}");
                        }
                    }
                    crikey_app::CatalogBuildResult::Failed { reason, .. } => {
                        eprintln!("crikey: native catalog build failed: {reason}");
                    }
                    crikey_app::CatalogBuildResult::Obsolete(_) => {}
                }
            }
            let registry_before = search.command_snapshot().revision;
            if let Some(configuration) = configuration.as_ref() {
                let disabled = DisabledPlugins::from_ids(configuration.store.disabled_plugins());
                for owner in native_driver.plugins().into_iter().chain(modern_driver.plugins()) {
                    if let Err(error) = search.set_command_owner_enabled(&owner, !disabled.blocks(&owner)) {
                        eprintln!("crikey: command owner policy intake refused: {error:?}");
                    }
                    if disabled.blocks(&owner) {
                        for request in pending_command_learning
                            .keys()
                            .chain(pending_typed_items.keys())
                            .filter(|request| request.plugin == owner)
                        {
                            search.cancel_action(request);
                        }
                    }
                }
            }
            for owner in native_driver.plugins() {
                if let Some(snapshot) = native_driver.command_snapshot(&owner) {
                    if let Err(error) = search.intake_native_commands(&snapshot) {
                        eprintln!("crikey: native command intake refused: {error:?}");
                    }
                }
            }
            let snapshot = modern_driver.command_snapshot();
            if let Err(error) = search.intake_modern_commands(&snapshot) {
                eprintln!("crikey: modern command intake refused: {error:?}");
            }
            command_projection_dirty |= registry_before != search.command_snapshot().revision;
            let registry = search.command_snapshot();
            let preferences = search.command_preferences_snapshot();
            let activation = settings::configured_hotkey(configuration.as_ref().map(|config| &config.store));
            let inputs = (registry.revision, preferences.revision, activation.clone());
            if hotkey_authoritative && hotkey_inputs.as_ref() != Some(&inputs) {
                hotkeys.reconcile(Arc::clone(&registry), Arc::clone(&preferences), Some(activation));
                hotkey_inputs = Some(inputs);
            }
            if let Some(report) = hotkeys.take_report() {
                for (owner, status) in &report.bindings {
                    if let Some(problem) = &status.problem {
                        let message = format!(
                            "Shortcut {owner:?}: desired {:?}, active {:?}: {problem:?}",
                            status.requested, status.active
                        );
                        eprintln!("crikey: {message}");
                        view_model.set_navigation_notice(message);
                        if *owner == crikey_app::BindingOwner::Launcher && status.active.is_none() {
                            let diagnostic = settings::surface_hotkey_failure(
                                &mut view_model,
                                status.requested.as_deref().unwrap_or("none"),
                                &format!("{problem:?}"),
                            );
                            if let Err(error) = append_startup_log(&diagnostic) {
                                eprintln!("crikey: startup log failed: {error}");
                            }
                        }
                    }
                }
                for failure in &report.cleanup_failures {
                    view_model.set_navigation_notice(format!(
                        "Shortcut cleanup {}: {}",
                        failure.accelerator, failure.error
                    ));
                }
                let mut rows = settings::rows(configuration.as_ref().map(|config| &config.store));
                if let Some(row) = rows
                    .iter_mut()
                    .find(|row| row.key == crikey_config::KEY_ACTIVATION_HOTKEY)
                {
                    row.label = format!(
                        "Activation hotkey · active: {}",
                        report
                            .bindings
                            .get(&crikey_app::BindingOwner::Launcher)
                            .and_then(|status| status.active.as_deref())
                            .unwrap_or("none")
                    );
                }
                view_model.set_settings(rows);
                hotkey_report = report;
                command_management_revision = None;
            }
            if view_model
                .inline_arguments()
                .is_some_and(|input| search.root_inline_binding(input).is_none())
            {
                view_model.retire_unavailable_inline_arguments();
            }
            let mut shortcut_root_query = None;
            for receipt in hotkey_receipts {
                let Some(dispatch) = hotkeys.resolve_event(&receipt, &registry, &preferences) else {
                    continue;
                };
                if view_model.shortcut_capture().is_some() && render_handle.is_visible() {
                    view_model.apply(crikey_ui::UiCommand::RecordShortcut(receipt.accelerator().into()));
                    continue;
                }
                hotkey_intent = hotkey_intent.checked_add(1).expect("shortcut intent exhausted");
                retained.explicit_input = None;
                shortcut_root_query = None;
                let typed_reference = match &dispatch {
                    crikey_app::HotkeyDispatch::OpenInput { reference }
                    | crikey_app::HotkeyDispatch::Execute { reference } => Some(reference),
                    crikey_app::HotkeyDispatch::Launcher => None,
                };
                if let Some(reference) = typed_reference {
                    if let Ok(Some(binding)) = search.root_command_binding(reference, registry.revision) {
                        let hidden = !render_handle.is_visible();
                        if !view_model.open_root_input() {
                            continue;
                        }
                        web_page.close("shortcut opened typed input");
                        search.close_page();
                        if hidden && render_handle.request_activation().is_ok() {
                            prepared_activation = true;
                        }
                        if let Some(row) = search
                            .command_management_rows()
                            .into_iter()
                            .find(|row| row.target == crikey_ui::ActionTarget::Command(reference.clone()))
                        {
                            let query = row.label.clone();
                            view_model.apply(crikey_ui::UiCommand::SetQuery(query.clone()));
                            retained.explicit_input = Some(row);
                            shortcut_root_query = Some(query);
                            open_typed_input(
                                &mut view_model,
                                &mut typed_revision,
                                &binding,
                                Some((reference.clone(), registry.revision)),
                            );
                        }
                        continue;
                    }
                }
                match dispatch {
                    crikey_app::HotkeyDispatch::Launcher => {
                        let _ = render_handle.request_toggle();
                    }
                    crikey_app::HotkeyDispatch::OpenInput { reference } => {
                        let hidden = !render_handle.is_visible();
                        activate_launcher_session(&mut search, &mut view_model, &mut web_page);
                        if hidden && render_handle.request_activation().is_ok() {
                            prepared_activation = true;
                        }
                        if let Some(row) = search
                            .command_management_rows()
                            .into_iter()
                            .find(|row| row.target == crikey_ui::ActionTarget::Command(reference.clone()))
                        {
                            let query = row.label.clone();
                            view_model.apply(crikey_ui::UiCommand::SetQuery(query.clone()));
                            retained.explicit_input = Some(row);
                            // Use the ordinary query effect so search, catalog
                            // pipeline, retained rows and model share one admission.
                            shortcut_root_query = Some(query);
                        }
                    }
                    crikey_app::HotkeyDispatch::Execute { reference } => {
                        match search.invoke_command(&reference, None, registry.revision) {
                            Ok(crikey_app::CommandSubmission::Host(
                                crikey_core::HostCommand::OpenSettings,
                            )) => {
                                if !render_handle.is_visible() {
                                    activate_launcher_session(&mut search, &mut view_model, &mut web_page);
                                    if render_handle.request_activation().is_ok() {
                                        prepared_activation = true;
                                    }
                                }
                                web_page.close("shortcut opened settings");
                                search.close_page();
                                view_model.open_settings(None);
                                if let Err(message) = publish_learning(
                                    command_history::Learning::Host(reference, history_clock.advance()),
                                    &mut search,
                                    &history_writer,
                                    ranking_reset_pending,
                                    &mut deferred_learning,
                                ) {
                                    view_model.set_navigation_notice(message);
                                }
                            }
                            Ok(crikey_app::CommandSubmission::Action {
                                receipt,
                                mut learning,
                            }) => {
                                // A process shortcut has no root-query affinity or argument text.
                                learning.query = None;
                                previews.retire_for_foreground(&mut view_model, &reference.provider_id);
                                if let ActionSubmission::Pending(id) = receipt.submission {
                                    pending_command_learning.insert(id.clone(), learning);
                                    hotkey_presentations.insert(id, hotkey_intent);
                                } else if let Err(message) = publish_learning(
                                    command_history::Learning::Command(learning, history_clock.advance()),
                                    &mut search,
                                    &history_writer,
                                    ranking_reset_pending,
                                    &mut deferred_learning,
                                ) {
                                    view_model.set_navigation_notice(message);
                                }
                            }
                            Err(error) => {
                                eprintln!("crikey: shortcut command refused: {error}");
                                if render_handle.is_visible() {
                                    view_model.set_navigation_notice(error.to_string());
                                }
                            }
                        }
                    }
                }
            }
            // A plugin at its declared `[concurrency]` limit is throttled, not
            // broken, and the two are indistinguishable from the outside
            // (spec 13.5, 24.3). Reporting the per-kind counters as they grow
            // is what lets an operator raise the right budget.
            for report in [
                legacy_driver.health_report(),
                modern_driver.health_report(),
                native_driver.health_report(),
            ] {
                report_concurrency_refusals(&mut reported_refusals, report);
            }

            let mut root_submission = false;
            let (command_session, effect) = match event {
                NativeLauncherEvent::Activated => {
                    // A rapid off/on hotkey pair may supersede the queued
                    // dismissal. Reset the old session before opening the new
                    // one so its query and rows cannot cross the boundary.
                    if prepared_activation {
                        prepared_activation = false;
                    } else {
                        retained.explicit_input = None;
                        activate_launcher_session(&mut search, &mut view_model, &mut web_page);
                    }
                    // A new session searches from cold. Within one burst of
                    // typing the filesystem is effectively still, but between
                    // two sessions the user has had time to create the file
                    // they are about to look for.
                    file_driver.invalidate_cache();
                    (None, None)
                }
                NativeLauncherEvent::Command { session, command } => {
                    root_submission = matches!(&command, crikey_ui::UiCommand::ExecuteDefault)
                        && (view_model.submitted_root_query().is_some()
                            || view_model.submitted_command_query().is_some());
                    let effect =
                        apply_native_command(&mut view_model, &mut retained, &mut hotkey_intent, command);
                    (Some(session), effect)
                }
                NativeLauncherEvent::TypedInput { session, submission } => {
                    if render_handle.is_visible_session(session)
                        && view_model.accepts_typed_draft(
                            submission.revision,
                            submission.draft_revision,
                            submission.view,
                            submission.generation,
                        )
                    {
                        if let Some(binding) = view_model
                            .inline_arguments()
                            .and_then(|input| search.root_inline_binding(input))
                        {
                            let command = view_model.inline_arguments().and_then(|input| {
                                input.command.as_ref().map(|(reference, _)| reference.clone())
                            });
                            match search.submit_root_typed(&binding, submission.values) {
                                Ok(crikey_app::TypedActionReceipt { receipt, item }) => {
                                    if let Some(reference) = command {
                                        pending_command_learning.insert(
                                            receipt.request_id.clone(),
                                            crikey_app::CommandLearning {
                                                reference,
                                                item,
                                                query: None,
                                            },
                                        );
                                    } else {
                                        pending_typed_items.insert(receipt.request_id.clone(), item);
                                    }
                                    typed_requests.insert(
                                        receipt.request_id,
                                        (
                                            submission.revision,
                                            submission.draft_revision,
                                            submission.view,
                                            submission.generation,
                                        ),
                                    );
                                    previews.retire_for_foreground(
                                        &mut view_model,
                                        &binding.executable.target.owner,
                                    );
                                }
                                Err(_) => view_model.inline_arguments_completed(
                                    submission.revision,
                                    submission.draft_revision,
                                    crikey_core::structured_arguments::SubmissionOutcome::Failed(
                                        crikey_core::structured_arguments::SubmissionFailureCode::Unavailable,
                                    ),
                                ),
                            }
                        } else {
                            view_model.inline_arguments_completed(
                                submission.revision,
                                submission.draft_revision,
                                crikey_core::structured_arguments::SubmissionOutcome::Failed(
                                    crikey_core::structured_arguments::SubmissionFailureCode::Unavailable,
                                ),
                            );
                        }
                    }
                    (Some(session), None)
                }
                // The wake that brought this turn about. Everything it asks
                // for — draining the drivers and republishing — happened
                // above, before the match, exactly as it does for every other
                // event; there is no command and no effect.
                NativeLauncherEvent::ProviderAnswer => (None, None),
                NativeLauncherEvent::ActionCompletion => (None, None),
            };

            // Read before the match consumes the effect: the window and the
            // process are one decision made in one place, so no arm can grow
            // its own private answer to "does this end the launcher".
            let disposition = effect.as_ref().and_then(settings::residency);
            // Leaving a covered root retires its old publications. Re-enter it
            // through ordinary query dispatch, after the page lifecycle effect
            // has run, and never duplicate a query already requested this turn.
            let open_shortcut_input = shortcut_root_query.is_some();
            let mut effect = effect.or_else(|| shortcut_root_query.map(UiEffect::Query));
            let mut registry_query_refresh =
                command_projection_revision != search.command_snapshot().revision;
            loop {
                match effect {
                    Some(UiEffect::RecallQuery) => {
                        view_model.set_query_recall(query_history.snapshot());
                    }
                    Some(UiEffect::ClearQueryHistory) => {
                        if let Err(error) = query_history.clear() {
                            let message = format!("Query history clear failed: {error}");
                            eprintln!("crikey: {message}");
                            view_model.set_navigation_notice(message);
                        }
                    }
                    Some(UiEffect::OpenPreviewLink { target, url }) => {
                        if let Err(reason) = previews.open_link(&target, &url, &search) {
                            view_model
                                .publish_preview(&target.selection, crikey_ui::PreviewStatus::Error(reason));
                        }
                    }
                    Some(UiEffect::Query(raw)) => {
                        registry_query_refresh = false;
                        // Both ranking inputs are refreshed immediately before the
                        // query is scored, because both describe the moment the
                        // user typed: recency is measured from now, and the
                        // foreground application is whatever they were working in
                        // when they reached for the launcher. Reading either once
                        // at startup would freeze it for the life of the process.
                        search.set_history_time(history_clock.advance());
                        search.refresh_foreground_category();
                        let now = u64::try_from(query_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
                        if let Ok((generation, catalog_result)) = admit_root_query(
                            &mut search,
                            &mut query_pipeline,
                            &owner,
                            &mut view_model,
                            &mut retained,
                            &raw,
                            now,
                        ) {
                            active_root_query = raw.clone();
                            command_projection_dirty = true;
                            retained.set_calculated(generation, calculator.rows(generation, &raw));
                            ordinary_catalog_state = if search.last_query_stats().matches_found == 0 {
                                crikey_app::OrdinaryProviderState::Empty
                            } else {
                                crikey_app::OrdinaryProviderState::Matches
                            };
                            // Dispatch never depends on synchronous intake succeeding.
                            // These workers owe independent answers for this generation.
                            // Never feed canonical catalog rows back through a
                            // provider outcome: catalog ownership overlaps with
                            // suggestion ownership, but these are distinct inputs.
                            let scope = view_model
                                .selected_scope()
                                .map(crikey_app::root_scope_from_identity);
                            let legacy_outstanding = scope.is_none() && legacy_driver.has_plugins();
                            if scope.is_none() {
                                legacy_driver.submit(generation, &raw, now, Vec::new(), false, 0);
                            } else {
                                legacy_driver.retire_query(generation);
                            }
                            let provider_query = ProviderQueryContext {
                                generation,
                                text: &raw,
                                now,
                                selected: 0,
                            };
                            let modern_outstanding = modern_driver.has_plugins()
                                && modern_driver
                                    .submit_scoped(provider_query, Vec::new(), false, scope.clone())
                                    .is_ok();
                            let native_outstanding = native_driver.has_plugins()
                                && native_driver
                                    .submit_scoped(provider_query, Vec::new(), false, scope.clone())
                                    .is_ok();
                            let files_outstanding = scope.is_none()
                                && !raw.trim().is_empty()
                                && file_driver.submit(generation, &raw, now);
                            retained.mark_pending(generation, RowSource::Legacy, legacy_outstanding);
                            retained.mark_pending(generation, RowSource::Native, native_outstanding);
                            retained.mark_pending(generation, RowSource::Modern, modern_outstanding);
                            retained.mark_pending(generation, RowSource::Files, files_outstanding);
                            publish_root_rows(
                                &search,
                                configuration.as_ref().map(|configuration| &configuration.store),
                                &mut retained,
                                &mut view_model,
                                generation,
                                &raw,
                                None,
                            );
                            if let Err(error) = catalog_result {
                                ordinary_catalog_state = crikey_app::OrdinaryProviderState::Failed;
                                report_status(
                                    &mut view_model,
                                    format!("Application catalog intake failed: {error}"),
                                );
                            }
                        }
                    }
                    // Hiding is the shared disposition below; the arm itself has
                    // nothing left to do.
                    Some(UiEffect::Dismissed) => {
                        // The window is going away, so the surface is too. Without
                        // this the plugin keeps a page session alive for a screen
                        // nobody is looking at, and goes on being asked to draw it
                        // (spec 32.10). `close_page` is inert when none is open.
                        web_page.close("launcher was dismissed");
                        search.close_page();
                    }
                    Some(UiEffect::Quit) => {
                        web_page.close("launcher is quitting");
                        for failure in hotkeys.shutdown() {
                            eprintln!(
                                "crikey: shortcut cleanup {}: {}",
                                failure.accelerator, failure.error
                            );
                        }
                    }
                    Some(UiEffect::Execute { .. })
                    | Some(UiEffect::ExecuteActionless { .. })
                    | Some(UiEffect::InvokeCommand { .. })
                        if ranking_reset_pending =>
                    {
                        view_model.set_navigation_notice(
                            "Ranking reset is pending; invoke again after it finishes".into(),
                        );
                    }
                    Some(UiEffect::ExecuteActionless {
                        owner,
                        item,
                        view,
                        generation,
                    }) => {
                        if view_model.view() == Some((view, crikey_ui::ViewKind::RootList))
                            && retained.generation() == Some(generation)
                        {
                            if let Some(binding) = search.root_result_binding(
                                &owner,
                                &item,
                                None,
                                view_model
                                    .selected_preview_row()
                                    .and_then(|row| row.source.as_ref()),
                            ) {
                                open_typed_input(&mut view_model, &mut typed_revision, &binding, None);
                            }
                        }
                    }
                    Some(UiEffect::InvokeCommand {
                        reference,
                        argument,
                        registry_revision,
                        view,
                        generation,
                        query,
                    }) => {
                        if view_model.view() != Some((view, crikey_ui::ViewKind::RootList))
                            || retained.generation() != Some(generation)
                            || query != active_root_query
                        {
                            view_model
                                .set_navigation_notice("Command selection changed; refresh and retry".into());
                        } else if let Ok(Some(binding)) =
                            search.root_command_binding(&reference, registry_revision)
                        {
                            if argument.is_some() {
                                view_model.set_navigation_notice(
                                    "Structured commands do not accept literal arguments".into(),
                                );
                            } else {
                                open_typed_input(
                                    &mut view_model,
                                    &mut typed_revision,
                                    &binding,
                                    Some((reference.clone(), registry_revision)),
                                );
                            }
                        } else {
                            match search.invoke_command(&reference, argument.as_deref(), registry_revision) {
                                Ok(crikey_app::CommandSubmission::Host(
                                    crikey_core::HostCommand::OpenSettings,
                                )) => {
                                    if let Some(notice) = command_failure_notice.take() {
                                        view_model.clear_navigation_notice(notice);
                                    }
                                    web_page.close("command opened settings");
                                    search.close_page();
                                    view_model.open_settings(None);
                                    let learning =
                                        command_history::Learning::Host(reference, history_clock.advance());
                                    command_projection_dirty = true;
                                    if let Err(message) = publish_learning(
                                        learning,
                                        &mut search,
                                        &history_writer,
                                        ranking_reset_pending,
                                        &mut deferred_learning,
                                    ) {
                                        view_model.set_navigation_notice(message);
                                    }
                                }
                                Ok(crikey_app::CommandSubmission::Action { receipt, learning }) => {
                                    if let Some(notice) = command_failure_notice.take() {
                                        view_model.clear_navigation_notice(notice);
                                    }
                                    previews.retire_for_foreground(&mut view_model, &reference.provider_id);
                                    if root_submission && argument.is_none() && receipt.query_history_allowed
                                    {
                                        if let Err(error) = query_history.record_submitted(&query) {
                                            eprintln!("crikey: query history: {error}");
                                        }
                                    }
                                    match receipt.submission {
                                        ActionSubmission::Completed => {
                                            let learning = command_history::Learning::Command(
                                                learning,
                                                history_clock.advance(),
                                            );
                                            command_projection_dirty = true;
                                            if let Err(message) = publish_learning(
                                                learning,
                                                &mut search,
                                                &history_writer,
                                                ranking_reset_pending,
                                                &mut deferred_learning,
                                            ) {
                                                view_model.set_navigation_notice(message);
                                            }
                                            view_model.dismiss();
                                            if let Some(session) = command_session {
                                                let _ = render_handle.request_hide_session(session);
                                            }
                                        }
                                        ActionSubmission::Pending(request_id) => {
                                            pending_command_learning.insert(request_id.clone(), learning);
                                            action_origins.insert(
                                                request_id.clone(),
                                                view_model.view().map(|(view, _)| view).zip(command_session),
                                            );
                                            report_status(
                                                &mut view_model,
                                                format!("Command pending (request {})", request_id.sequence),
                                            );
                                        }
                                    }
                                }
                                Err(error) => {
                                    command_failure_notice = view_model.publish_navigation_notice(format!(
                                        "Command {} / {} was not invoked: {error}",
                                        reference.provider_id.0, reference.command_id
                                    ));
                                }
                            }
                        }
                    }
                    Some(UiEffect::MutateCommandPreferences {
                        request_id,
                        reference,
                        expected_revision,
                        edit,
                    }) => {
                        let snapshot = search.command_preferences_snapshot();
                        let failed = crikey_ui::CommandPreferenceEditOutcome {
                            request_id,
                            reference: reference.clone(),
                            edit: edit.clone(),
                            status: crikey_ui::SettingEditStatus::Failed,
                        };
                        let result = if snapshot.revision != expected_revision {
                            Err("Command preferences changed; refresh and retry this edit".to_owned())
                        } else if matches!(edit, crikey_ui::CommandPreferenceEdit::Fallback(true))
                            && search
                                .root_command_binding(&reference, search.command_snapshot().revision)
                                .ok()
                                .flatten()
                                .is_some()
                        {
                            Err("Typed commands cannot be enrolled as literal text fallbacks".to_owned())
                        } else {
                            configuration
                                .as_ref()
                                .ok_or_else(|| {
                                    "Configuration authority unavailable; command preferences were not saved"
                                        .to_owned()
                                })
                                .and_then(|configuration| {
                                    preference_worker
                                        .submit_mutation(command_preferences::MutationRequest {
                                            request_id,
                                            config: configuration.store.clone(),
                                            registry: search.command_snapshot(),
                                            expected_revision,
                                            reference,
                                            edit,
                                            session: false,
                                        })
                                        .map_err(|(_, message)| message)
                                })
                        };
                        if let Err(message) = result {
                            view_model.command_edit_completed(failed);
                            view_model.set_navigation_notice(message);
                        }
                    }
                    Some(UiEffect::ResetCommandRanking {
                        reference,
                        registry_revision,
                    }) => {
                        let result = if ranking_reset_pending {
                            Err("A ranking reset is already pending".to_owned())
                        } else {
                            search
                                .command_ranking_target(&reference, registry_revision)
                                .map_err(|error| error.to_string())
                                .and_then(|(owner, item)| {
                                    history_writer.reset(search.selection_history_snapshot(), owner, item)
                                })
                        };
                        match result {
                            Ok(()) => {
                                // New execution admission stays closed until
                                // acknowledgment, bounding deferred successes
                                // by these already admitted exact request IDs.
                                deferred_learning.reserve(
                                    pending_command_learning
                                        .len()
                                        .saturating_add(pending_ordinary_learning.len())
                                        .saturating_add(pending_typed_items.len()),
                                );
                                ranking_reset_pending = true;
                                search.set_history_reset_pending(true);
                            }
                            Err(message) => view_model.set_navigation_notice(message),
                        }
                    }
                    Some(UiEffect::SetSetting {
                        request_id,
                        key,
                        value,
                    }) => {
                        if let Some(configuration) = configuration.as_ref() {
                            if let Err((outcome, message)) = preference_worker.submit_setting(
                                request_id,
                                configuration.store.clone(),
                                key,
                                value,
                            ) {
                                view_model.setting_edit_completed(outcome);
                                view_model.set_navigation_notice(message);
                            }
                        } else {
                            view_model.setting_edit_completed(crikey_ui::SettingEditOutcome {
                                request_id,
                                key,
                                value,
                                status: crikey_ui::SettingEditStatus::Failed,
                            });
                            view_model.set_navigation_notice(
                                "Configuration authority unavailable; setting was not saved".into(),
                            );
                        }
                    }
                    Some(UiEffect::PageInput(input)) => {
                        // A page that has stopped answering is reported once, on
                        // the input that discovered it, rather than leaving the
                        // user pressing keys at a surface nobody owns.
                        if let Err(error) = search.send_page_input(input) {
                            page_failure_status = Some(close_failed_web_page(
                                &mut web_page,
                                &mut search,
                                &mut view_model,
                                error.to_string(),
                            ));
                        }
                    }
                    #[cfg(target_os = "linux")]
                    Some(UiEffect::WebInput(input)) => {
                        if let Err(error) = web_page.send_input(input) {
                            page_failure_status = Some(close_failed_web_page(
                                &mut web_page,
                                &mut search,
                                &mut view_model,
                                error,
                            ));
                        }
                    }
                    #[cfg(not(target_os = "linux"))]
                    Some(UiEffect::WebInput(_)) => {}
                    #[cfg(target_os = "linux")]
                    Some(UiEffect::WebIme(event)) => {
                        if let Err(error) = web_page.send_ime(event) {
                            page_failure_status = Some(close_failed_web_page(
                                &mut web_page,
                                &mut search,
                                &mut view_model,
                                error,
                            ));
                        }
                    }
                    #[cfg(not(target_os = "linux"))]
                    Some(UiEffect::WebIme(_)) => {}
                    Some(UiEffect::ResizePage { width, height }) => {
                        search.resize_page(width, height);
                        #[cfg(target_os = "linux")]
                        if let Err(error) = web_page.resize(width, height) {
                            page_failure_status = Some(close_failed_web_page(
                                &mut web_page,
                                &mut search,
                                &mut view_model,
                                error,
                            ));
                        }
                    }
                    Some(UiEffect::ClosePage) => {
                        // The model has already forgotten the page; this is what
                        // tells the plugin, and it runs even when the model had
                        // nothing open so a session can never outlive its surface.
                        web_page.close("page was closed");
                        search.close_page();
                    }
                    Some(UiEffect::Execute { owner, item, action }) => {
                        // A calculated row is resolved first and settled entirely
                        // here. Its value never entered the catalog and copying it
                        // is not one of the three host-mediated catalog actions, so
                        // neither `execute` nor `execute_host_action` could do
                        // anything but refuse it; the calculator owns the clipboard
                        // capability and makes the call itself.
                        let calculated = retained
                            .generation()
                            .filter(|_| {
                                owner.0 == calculator::CALCULATOR_PLUGIN
                                    && action.0 == "crikey.calculator.copy"
                            })
                            .and_then(|generation| calculator.resolve(generation, &item))
                            .map(str::to_owned);
                        if let Some(binding) = search.root_result_binding(
                            &owner,
                            &item,
                            Some(&action),
                            view_model
                                .selected_preview_row()
                                .and_then(|row| row.source.as_ref()),
                        ) {
                            open_typed_input(&mut view_model, &mut typed_revision, &binding, None);
                        } else if let Some(text) = calculated {
                            match calculator.copy(&text) {
                                Ok(()) => {
                                    view_model.dismiss();
                                    if let Some(session) = command_session {
                                        let _ = render_handle.request_hide_session(session);
                                    }
                                }
                                // The launcher stays open on a failed copy: the
                                // result is still on screen for the user to read,
                                // which is the only thing left that can be done
                                // for them.
                                Err(error) => {
                                    report_status(&mut view_model, format!("Copy failed: {error}"));
                                }
                            }
                        } else {
                            // Everything else — catalog item, plugin item, or a
                            // file the search merged into this generation's answer
                            // — is looked up in that answer. A file item is in
                            // `results` like any other now, so `execute` finds it,
                            // applies the same host-mediated gate, and
                            // `record_selection` below can finally record having
                            // opened it.
                            let learning = search.selection_learning(&owner, &item);
                            let submission = search.execute_owned(&owner, &item, &action).map(|receipt| {
                                if let Some(notice) = command_failure_notice.take() {
                                    view_model.clear_navigation_notice(notice);
                                }
                                if matches!(&receipt.submission, ActionSubmission::Pending(_)) {
                                    previews.retire_for_foreground(&mut view_model, &owner);
                                }
                                if root_submission && receipt.query_history_allowed {
                                    if let Some((query, _, _)) = view_model.submitted_root_query() {
                                        if let Err(error) = query_history.record_submitted(query) {
                                            eprintln!("crikey: query history: {error}");
                                        }
                                    }
                                }
                                receipt.submission
                            });
                            match submission {
                                Ok(ActionSubmission::Completed) => {
                                    if let Some(learning) = learning {
                                        command_projection_dirty = true;
                                        let learning = command_history::Learning::Ordinary(
                                            learning,
                                            history_clock.advance(),
                                        );
                                        if let Err(message) = publish_learning(
                                            learning,
                                            &mut search,
                                            &history_writer,
                                            ranking_reset_pending,
                                            &mut deferred_learning,
                                        ) {
                                            view_model.set_navigation_notice(message);
                                        }
                                    }
                                    view_model.dismiss();
                                    if let Some(session) = command_session {
                                        let _ = render_handle.request_hide_session(session);
                                    }
                                }
                                Ok(ActionSubmission::Pending(request_id)) => {
                                    if let Some(learning) = learning {
                                        pending_ordinary_learning.insert(request_id.clone(), learning);
                                    }
                                    action_origins.insert(
                                        request_id.clone(),
                                        view_model.view().map(|(view, _)| view).zip(command_session),
                                    );
                                    let message = format!(
                                        "Action pending ({} / request {})",
                                        request_id.plugin.0, request_id.sequence
                                    );
                                    report_status(&mut view_model, message);
                                }
                                Err(error) => {
                                    report_status(&mut view_model, format!("Launch failed: {error}"));
                                }
                            }
                        }
                    }
                    None => {}
                }
                if let Some(route @ crikey_ui::NavigationRoute::CustomPage { .. }) =
                    view_model.take_page_reopen()
                {
                    if let crikey_ui::NavigationRoute::CustomPage { plugin, page_id, .. } = &route {
                        if let Err(reason) = open_plugin_page(
                            &mut search,
                            &mut view_model,
                            &mut web_page,
                            plugin,
                            page_id,
                            page_window,
                        ) {
                            view_model.page_reopen_failed(route, reason);
                        }
                    }
                }
                effect = match view_model.take_query_refresh() {
                    Some(raw) => {
                        registry_query_refresh = false;
                        Some(UiEffect::Query(raw))
                    }
                    None if registry_query_refresh
                        && view_model
                            .view()
                            .is_some_and(|(_, kind)| kind == crikey_ui::ViewKind::RootList) =>
                    {
                        registry_query_refresh = false;
                        view_model.refresh_root_query().map(UiEffect::Query)
                    }
                    None => break,
                };
            }
            if open_shortcut_input
                && retained.explicit_input.is_some()
                && view_model
                    .view()
                    .is_some_and(|(_, kind)| kind == crikey_ui::ViewKind::RootList)
            {
                view_model.apply(crikey_ui::UiCommand::ShowActions);
            }
            // Returning from a failed page republishes root rows synchronously.
            // Attach its diagnostic only afterwards, so that publication cannot
            // erase the message before the next graphical frame.
            if let Some(message) = page_failure_status {
                report_status(&mut view_model, message);
            }

            // The launcher is resident: a dismiss hides the window and the
            // process goes on waiting for its activation hotkey, and only an
            // explicit quit tears it down. Exiting here is what releases the
            // launcher lock and joins every provider supervisor, so quitting
            // through the event loop rather than the process leaves nothing
            // behind for the next launch to trip over.
            match disposition {
                Some(settings::Residency::Hide) => {
                    if let Some(session) = command_session {
                        let _ = render_handle.request_hide_session(session);
                    }
                }
                Some(settings::Residency::Exit) => {
                    quit_after_history_reset = true;
                }
                None => {}
            }
            if quit_after_history_reset && !ranking_reset_pending {
                let _ = render_handle.request_exit();
            }

            command_projection_dirty |= command_projection_revision != search.command_snapshot().revision;
            let management_revision = (
                search.command_snapshot().revision,
                search.command_preferences_snapshot().revision,
            );
            if command_management_revision != Some(management_revision) {
                let mut rows = search.command_management_rows();
                for row in &mut rows {
                    if let crikey_ui::ActionTarget::Command(reference) = &row.target {
                        let status = hotkey_report
                            .bindings
                            .get(&crikey_app::BindingOwner::Command(reference.clone()));
                        row.status = Some(format!(
                            "Desired: {} · Active: {} · Source: {}{}",
                            row.command_state
                                .as_ref()
                                .and_then(|state| state.hotkey.as_deref())
                                .unwrap_or("none"),
                            status
                                .and_then(|status| status.active.as_deref())
                                .unwrap_or("none"),
                            preferences.layer.map_or("default", |layer| layer.as_str()),
                            status
                                .and_then(|status| status.problem.as_ref())
                                .map_or(String::new(), |problem| format!(" · {problem:?}"))
                        ));
                    }
                }
                view_model.set_command_management_rows(rows);
                command_management_revision = Some(management_revision);
            }
            if let (Some((view, crikey_ui::ViewKind::RootList)), Some(generation)) =
                (view_model.view(), retained.generation())
            {
                let evidence = vec![
                    Some(query_pipeline.ordinary_search_summary(view.get())),
                    legacy_driver.ordinary_search_summary(view.get()),
                    modern_driver.ordinary_search_summary(view.get()),
                    native_driver.ordinary_search_summary(view.get()),
                    if active_root_query.trim().is_empty() {
                        Some(crikey_app::OrdinarySearchSummary {
                            view: view.get(),
                            generation,
                            providers: Vec::new(),
                        })
                    } else {
                        file_driver.ordinary_search_summary(view.get())
                    },
                    calculator.ordinary_search_summary(view.get()),
                ];
                command_projection_dirty |= evidence != last_ordinary_evidence;
                if command_projection_dirty {
                    let fallback = compose_fallback_search(
                        &active_root_query,
                        view,
                        generation,
                        ordinary_catalog_state,
                        &evidence,
                    );
                    retained.set_builtin(generation, search.result_rows());
                    publish_root_rows(
                        &search,
                        configuration.as_ref().map(|configuration| &configuration.store),
                        &mut retained,
                        &mut view_model,
                        generation,
                        &active_root_query,
                        fallback.as_ref(),
                    );
                    last_ordinary_evidence = evidence;
                    command_projection_revision = search.command_snapshot().revision;
                    command_projection_dirty = false;
                }
            }
            previews.synchronize(&mut view_model, &search);
            if let Some(frame) = view_model.frame() {
                let _ = render_handle.submit_frame(&frame);
            }
        }))
        .map_err(|error| error.to_string());
    // Only a deliberate exit clears the record; a run that ended in an error
    // leaves its plugin set on disk for the next launch to read.
    if outcome.is_ok() {
        ledger.borrow_mut().mark_clean_shutdown();
    }
    outcome
}
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct ActiveWebSurface {
    surface_id: u64,
    plugin: PluginId,
    storage: WebStorage,
}

struct WebPageRuntime {
    #[cfg(target_os = "linux")]
    supervisor: Option<WebHostSupervisor>,
    #[cfg(target_os = "linux")]
    storage_root: std::path::PathBuf,
    #[cfg(target_os = "linux")]
    wake: WebWake,
    #[cfg(target_os = "linux")]
    active: Option<ActiveWebSurface>,
    #[cfg(target_os = "linux")]
    viewport: (u32, u32),
}

impl WebPageRuntime {
    #[cfg(target_os = "linux")]
    fn new(storage_root: std::path::PathBuf, wake: WebWake, viewport: (u32, u32)) -> Self {
        Self {
            supervisor: None,
            storage_root,
            wake,
            active: None,
            viewport,
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn new() -> Self {
        Self {}
    }

    #[cfg(target_os = "linux")]
    fn begin_page(&mut self, viewport: (u32, u32)) {
        self.close("page was replaced");
        self.viewport = viewport;
    }

    #[cfg(not(target_os = "linux"))]
    fn begin_page(&mut self, _viewport: (u32, u32)) {}

    #[cfg(target_os = "linux")]
    fn declare(
        &mut self,
        search: &SearchService,
        view_model: &mut LauncherViewModel,
        plugin: &PluginId,
        declaration: &PageWebSurface,
    ) -> Result<(), String> {
        search
            .admit_web_surface(
                plugin,
                !declaration.url.is_empty(),
                declaration.storage == WebStorage::Persistent,
            )
            .map_err(|error| error.to_string())?;

        if let Some(active) = self.active.as_ref() {
            if active.plugin != *plugin {
                return Err(format!(
                    "web surface {} belongs to `{}`, not `{}`",
                    active.surface_id, active.plugin.0, plugin.0
                ));
            }
            if active.storage != declaration.storage {
                return Err("a web surface cannot change its storage mode while it is open".to_owned());
            }
            if declaration.url.is_empty() {
                return Ok(());
            }
            return self
                .supervisor
                .as_ref()
                .ok_or_else(|| "the web-host supervisor is unavailable".to_owned())?
                .navigate(active.surface_id, declaration.url.clone());
        }

        if declaration.url.is_empty() {
            return Err("the first web-surface declaration must name an http or https address".to_owned());
        }
        let supervisor = self
            .supervisor
            .get_or_insert_with(|| WebHostSupervisor::new(self.storage_root.clone(), Arc::clone(&self.wake)));
        let surface_id = supervisor.open(
            plugin.clone(),
            self.viewport.0,
            self.viewport.1,
            declaration.url.clone(),
            declaration.storage,
        )?;
        if !view_model.declare_web_surface(surface_id) {
            supervisor.close("the page no longer accepts the admitted web surface");
            return Err("the page no longer accepts its admitted web surface".to_owned());
        }
        self.active = Some(ActiveWebSurface {
            surface_id,
            plugin: plugin.clone(),
            storage: declaration.storage,
        });
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn declare(
        &mut self,
        _search: &SearchService,
        _view_model: &mut LauncherViewModel,
        _plugin: &PluginId,
        _declaration: &PageWebSurface,
    ) -> Result<(), String> {
        Err(
            "web surfaces require Linux and an installed WPE web host; this launcher remains available without them"
                .to_owned(),
        )
    }

    #[cfg(target_os = "linux")]
    fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        self.viewport = (width, height);
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        self.supervisor
            .as_ref()
            .ok_or_else(|| "the web-host supervisor is unavailable".to_owned())?
            .resize(active.surface_id, width, height)
    }

    #[cfg(target_os = "linux")]
    fn send_input(&self, input: crikey_ui::WebSurfaceInput) -> Result<(), String> {
        self.supervisor
            .as_ref()
            .ok_or_else(|| "the web-host supervisor is unavailable".to_owned())?
            .send_input(input)
    }

    #[cfg(target_os = "linux")]
    fn send_ime(&self, event: crikey_ui::WebImeEvent) -> Result<(), String> {
        self.supervisor
            .as_ref()
            .ok_or_else(|| "the web-host supervisor is unavailable".to_owned())?
            .send_ime(event)
    }

    #[cfg(target_os = "linux")]
    fn poll(&self) -> Vec<WebHostNotice> {
        self.supervisor
            .as_ref()
            .map_or_else(Vec::new, WebHostSupervisor::poll)
    }

    #[cfg(target_os = "linux")]
    fn close(&mut self, reason: &str) {
        self.active = None;
        if let Some(supervisor) = self.supervisor.as_ref() {
            supervisor.close(reason);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn close(&mut self, _reason: &str) {}
}

fn open_typed_input(
    model: &mut LauncherViewModel,
    sequence: &mut u64,
    binding: &crikey_app::RootExecutableBinding,
    command: Option<(crikey_core::CommandRef, u64)>,
) -> bool {
    let Some(revision) = sequence.checked_add(1) else {
        model.set_navigation_notice("Command input identity limit reached; restart the launcher.".into());
        return false;
    };
    *sequence = revision;
    model.open_inline_arguments(
        revision,
        binding.executable.clone(),
        binding.schema.clone(),
        crikey_app::root_publication_identity(&binding.source),
        command,
    )
}

fn activate_launcher_session(
    search: &mut SearchService,
    view_model: &mut LauncherViewModel,
    web_page: &mut WebPageRuntime,
) {
    // Native activation can supersede a queued dismissal before the event
    // loop receives it. Retire every old page owner, not just its model.
    web_page.close("launcher session was replaced");
    search.close_page();
    view_model.dismiss();
    view_model.activate();
}

/// Retires the failed page and returns the diagnostic to report after the
/// caller drains the root query refresh, before presenting its next frame.
fn close_failed_web_page(
    runtime: &mut WebPageRuntime,
    search: &mut SearchService,
    view_model: &mut LauncherViewModel,
    message: String,
) -> String {
    runtime.close("web surface failed");
    search.close_page();
    view_model.close_page();
    format!("Page closed: {message}")
}

/// Opens a plugin-drawn page and puts it on screen.
///
/// First admit the bounded logical route, then close old resources and open
/// a fresh page session. Failure leaves a host-owned unavailable route with Back,
/// never an orphaned session or an action replay.
fn open_plugin_page(
    search: &mut SearchService,
    view_model: &mut LauncherViewModel,
    web_page: &mut WebPageRuntime,
    plugin: &PluginId,
    page_id: &str,
    window: (u32, u32),
) -> Result<(), String> {
    let (width, height) = initial_page_viewport(window.0, window.1);
    // The submission left "Action pending" on the row. The action did not stay
    // pending, it became a page; leaving the message there would go on telling
    // the user to wait after the page has opened and closed again.
    let _ = view_model.set_selected_status(String::new());
    // Empty until the plugin answers: the host draws the sheet, so an unfilled
    // page is a blank panel rather than a hole. `stale` says the host is
    // waiting, and `answered` says nothing has ever been drawn here, which is
    // what earns the loading state on the sheet rather than only in the
    // footer.
    let shown = view_model.open_page(PageSurface {
        plugin: plugin.clone(),
        page_id: page_id.to_owned(),
        plugin_name: plugin.0.clone(),
        frame: Arc::new(crikey_core::PageFrame::default()),
        stale: true,
        answered: false,
        web_surface_id: None,
        web_frame: None,
        web_generation: 0,
        web_caret_area: None,
    });
    if !shown {
        return Ok(());
    }
    // Admit the logical entry before retiring any live resource. Refusal at the
    // navigation bound must leave the existing page usable.
    web_page.begin_page((width, height));
    if let Err(error) = search.open_page(plugin, page_id, width, height, page_palette()) {
        let reason = error.to_string();
        view_model.page_reopen_failed(
            crikey_ui::NavigationRoute::CustomPage {
                plugin: plugin.clone(),
                page_id: page_id.into(),
                title: plugin.0.clone(),
            },
            reason.clone(),
        );
        return Err(reason);
    }
    Ok(())
}

/// Tells the operator about an action's outcome and puts it on the selected
/// result.
///
/// The launcher can only show the message when a result is selected, and the
/// list may have emptied while the action was running. In that case the
/// message would otherwise vanish, so say plainly that it is not on screen
/// rather than leaving the operator to wonder why nothing appeared.
fn report_status(view_model: &mut LauncherViewModel, message: String) {
    eprintln!("crikey: {message}");
    if !view_model.set_selected_status(message) {
        eprintln!("crikey: no result is selected, so the message above is not shown in the launcher");
    }
}

/// Announces every §13.5 concurrency refusal that has appeared since the last
/// turn, naming the kind of work that was turned away.
///
/// The per-kind breakdown is the point: refused catalog builds mean the plugin
/// declared too small a `max-catalog-tasks` for the rebuild the launcher asks
/// for at startup, while refused suggestions mean the user is typing faster
/// than the plugin can answer. Reporting only a total would tell an operator
/// that something is throttled without telling them what to change.
fn report_concurrency_refusals(
    reported: &mut std::collections::BTreeMap<PluginId, crikey_app::ConcurrencyRefusals>,
    health: Vec<(PluginId, crikey_app::PluginHealth)>,
) {
    for (plugin, health) in health {
        let observed = health.concurrency_refusals;
        let previous = reported.get(&plugin).copied().unwrap_or_default();
        if observed == previous {
            continue;
        }
        for kind in crikey_app::BudgetKind::ALL {
            let delta = observed.of(kind).saturating_sub(previous.of(kind));
            if delta != 0 {
                eprintln!(
                    "crikey: plugin `{}` refused {delta} {} unit(s) at its declared concurrency limit",
                    plugin.0,
                    refusal_kind_name(kind)
                );
            }
        }
        reported.insert(plugin, observed);
    }
}

/// The manifest spelling of a budget kind, so the diagnostic names the key the
/// operator would edit.
fn refusal_kind_name(kind: crikey_app::BudgetKind) -> &'static str {
    match kind {
        crikey_app::BudgetKind::Suggestion => "suggestion",
        crikey_app::BudgetKind::Action => "action",
        crikey_app::BudgetKind::Background => "background",
        crikey_app::BudgetKind::Catalog => "catalog",
    }
}

/// Reads a platform path-list environment variable as plugin roots.
///
/// Empty components are ignored instead of becoming the current directory,
/// and duplicate roots are removed while preserving the operator's order.
/// Scanning one root twice would try to register every plugin twice and turn a
/// harmless repeated path into a spurious duplicate-plugin diagnostic.
fn configured_plugin_roots(variable: &str) -> Vec<std::path::PathBuf> {
    let Some(value) = std::env::var_os(variable) else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for root in std::env::split_paths(&value) {
        if root.as_os_str().is_empty() || roots.contains(&root) {
            continue;
        }
        roots.push(root);
    }
    roots
}

/// Builds the remote catalog service from configuration (spec 2.2, ADR-0016).
///
/// Every failure below degrades to "no remote source" or "no trusted key" and
/// is reported, never fatal: a launcher that refused to start because a shared
/// index was misdeclared would be a worse launcher than one that starts without
/// it. An empty declaration is the default and produces a service that does
/// nothing at all.
fn remote_catalog_service(
    directories: &StandardDirectories,
    configuration: Option<&LauncherConfiguration>,
) -> RemoteCatalogService {
    let idle = |sources| {
        RemoteCatalogService::new(
            sources,
            Arc::new(DefaultCatalogFetcher),
            Arc::new(crikey_package_manager::TrustStore::empty()),
        )
    };
    let Some(configuration) = configuration else {
        return idle(Vec::new());
    };
    let declared = match crikey_config::remote_catalog_sources(&configuration.store) {
        Ok(declared) => declared,
        Err(error) => {
            eprintln!("crikey: remote catalog sources unavailable: {error}");
            return idle(Vec::new());
        }
    };
    if declared.is_empty() {
        return idle(Vec::new());
    }
    let sources: Vec<RemoteSource> = declared
        .iter()
        .map(|declared| {
            let mut source = RemoteSource::new(&declared.name, &declared.url);
            source.interval_ms = declared.interval_ms;
            source.max_bytes = declared.max_bytes;
            source.require_signature = declared.require_signature;
            source.signing_key = declared.signing_key.clone();
            source
        })
        .collect();
    // An unreadable trust store leaves an empty one, which refuses every source
    // that requires a signature by name rather than admitting it unchecked.
    let trust = match crikey_package_manager::TrustStore::load(directories) {
        Ok(trust) => trust,
        Err(error) => {
            eprintln!(
                "crikey: trusted key store unavailable: {error}; \
                 remote catalog sources requiring a signature will be refused"
            );
            crikey_package_manager::TrustStore::empty()
        }
    };
    RemoteCatalogService::new(sources, Arc::new(DefaultCatalogFetcher), Arc::new(trust))
}

/// The launcher's live view of the layered configuration (spec 21).
struct LauncherConfiguration {
    store: ConfigStore,
    publisher: ConfigurationPublisher,
    reload_interval: Duration,
    checked: Instant,
    /// Set when a reload replaced [`Self::store`], and cleared by
    /// [`Self::take_reloaded`].
    ///
    /// The launcher's own settings — the activation hotkey among them — are not
    /// published to plugins, so they are invisible to the publisher's coalesced
    /// state and need their own edge. One flag rather than a comparison of the
    /// two stores: the question is "did the file change", and the file changing
    /// is exactly what this records.
    reloaded: bool,
}

impl LauncherConfiguration {
    fn load(
        directories: &StandardDirectories,
        session_overrides: &[(String, String)],
    ) -> Result<Self, String> {
        Self::load_store(directories, session_overrides).map(Self::from_store)
    }

    fn load_store(
        directories: &StandardDirectories,
        session_overrides: &[(String, String)],
    ) -> Result<ConfigStore, String> {
        let policy = administrator_policy_path(DirectoryConvention::current());
        let mut store =
            ConfigStore::load_with_overrides(directories, Some(policy.as_path()), session_overrides)
                .map_err(|_| "cannot load the configuration".to_owned())?;
        let problems = config_commands::register_schemas(&mut store, directories);
        if !problems.is_empty() {
            eprintln!(
                "crikey: {} plugin configuration schema declaration(s) unavailable",
                problems.len()
            );
        }
        Ok(store)
    }

    fn from_store(store: ConfigStore) -> Self {
        let publisher = ConfigurationPublisher::new(
            millis(&store, KEY_COALESCE_MS),
            millis(&store, KEY_MAXIMUM_WAIT_MS),
        );
        Self {
            reload_interval: millis(&store, KEY_RELOAD_INTERVAL_MS),
            store,
            publisher,
            checked: Instant::now(),
            reloaded: false,
        }
    }

    fn seed(&mut self, now: Instant) {
        self.publisher.observe(self.store.configuration_snapshot(), now);
    }

    /// Whether the store has been replaced since this was last asked.
    fn take_reloaded(&mut self) -> bool {
        std::mem::take(&mut self.reloaded)
    }

    fn poll(
        &mut self,
        now: Instant,
        worker: &command_preferences::PreferenceWorker,
    ) -> Option<crikey_app::PluginConfiguration> {
        if now.duration_since(self.checked) >= self.reload_interval
            && worker.submit(command_preferences::Request::Poll).is_ok()
        {
            self.checked = now;
        }
        let coalesced = self.publisher.coalesced();
        let state = self.publisher.poll(now)?;
        if coalesced > 0 {
            eprintln!("crikey: configuration reloaded; {coalesced} intermediate edit(s) coalesced");
        }
        Some(state.plugins().clone())
    }
}

/// A duration from a millisecond-valued configuration key.
///
/// An unparseable value falls back to the built-in default rather than failing the
/// launch, and says so: a timing hint is not worth refusing to start over.
fn millis(store: &ConfigStore, key: &str) -> Duration {
    let built_in = crikey_config::BUILT_IN_DEFAULTS
        .iter()
        .find(|(name, _)| *name == key)
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .unwrap_or(0);
    match store.get(key) {
        None => Duration::from_millis(built_in),
        Some(text) => match text.parse::<u64>() {
            Ok(value) => Duration::from_millis(value),
            Err(_) => {
                eprintln!("crikey: `{key}` is not a whole number of milliseconds; using {built_in}");
                Duration::from_millis(built_in)
            }
        },
    }
}

/// The user's query aliases, as configured (spec 21.2).
///
/// Written as an `[aliases]` table, one entry per abbreviation:
///
/// ```toml
/// [aliases]
/// ss = "Settings"
/// vsc = "Visual Studio Code"
/// ```
fn configured_aliases(store: &ConfigStore) -> AliasTable {
    AliasTable::new(store.aliases())
}

/// The pipeline bounds this launch runs under, with `launcher.max-results`
/// applied (spec 21.2).
///
/// The one launcher-wide setting that observably changes a running launcher:
/// it caps the results one query may produce across all plugins, so lowering it
/// in `config.toml` is visible in the next query's row count. The aggregator's
/// own default stands when the key is absent, which is why this crate does not
/// carry a second copy of that number.
fn pipeline_config(configuration: Option<&LauncherConfiguration>) -> PipelineConfig {
    pipeline_config_store(configuration.map(|configuration| &configuration.store))
}

fn pipeline_config_store(store: Option<&ConfigStore>) -> PipelineConfig {
    let mut config = PipelineConfig::default();
    let Some(text) = store.and_then(|store| store.get(KEY_MAX_RESULTS)) else {
        return config;
    };
    match text.parse::<usize>() {
        Ok(0) | Err(_) => {
            eprintln!(
                "crikey: `{KEY_MAX_RESULTS}` must be a positive whole number; \
                 using the built-in ceiling of {}",
                config.limits.max_items_per_query
            );
        }
        Ok(limit) => {
            config.limits.max_items_per_query = limit;
            // The intake queue's item capacity is derived from the same ceiling in
            // `PipelineConfig::default`; leaving it behind would let the queue
            // accept more than the aggregator will ever merge.
            config.intake_limits.capacity_items = limit;
        }
    }
    config
}

/// Hands one complete configuration state to every provider that can carry it.
///
/// Modern and native only. The Legacy Compatibility Layer keeps Keypirinha
/// configuration syntax and its own notification contract (spec 21.1 last line,
/// spec 14); routing a legacy plugin's settings through this store would change
/// the format its own configuration path already reads.
fn publish_configuration(
    modern: &ModernDriver,
    native: &NativeDriver,
    state: crikey_app::PluginConfiguration,
) {
    modern.publish_configuration(state.clone());
    native.publish_configuration(state);
}

/// Every root a provider scans for `kind`: the operator's `CRIKEY_*_ROOTS`
/// first, then the standard directory `crikey plugin install` writes to.
///
/// Both, in that order, and this is the seam that makes `crikey plugin install`
/// mean anything: an installed plugin lives under
/// [`StandardDirectories::plugin_dir`] and would never be discovered if the
/// launcher only scanned the environment variables. The environment keeps
/// precedence so a developer pointing the launcher at a working tree still
/// shadows their installed copy, and duplicates are removed so one path named
/// twice is not a spurious duplicate-plugin diagnostic.
pub(crate) fn discovery_roots(
    kind: PluginKind,
    configured: Vec<std::path::PathBuf>,
    directories: &StandardDirectories,
) -> Vec<std::path::PathBuf> {
    let mut roots = configured;
    let installed = directories.plugin_dir(kind);
    // A standard install directory that does not exist is a fresh profile with
    // nothing installed yet — the ordinary first-run state, not a failure.
    // Handing it to a provider anyway opened `crikey run` with `cannot scan
    // modern plugin root: No such file or directory` on a correct install,
    // which reads as a broken download, contradicts `crikey plugin doctor`
    // calling the same profile healthy, and drowns the scan failures that do
    // mean something. `crikey_config::discover_plugin_schemas` already takes
    // this view of a missing root, so this makes the runtimes agree.
    //
    // Only a definite absence is dropped. `try_exists` reports an error when
    // the answer is unknown — a parent that cannot be traversed, say — and that
    // root is still handed over, so a permission problem or a file where the
    // directory should be is reported exactly as loudly as before. A root the
    // operator named themselves is never dropped either: a directory they named
    // and does not exist is their mistake to hear about. Nothing is created
    // here; scanning must not have the side effect of installing.
    if !roots.iter().any(|root| root == &installed) && installed.try_exists().unwrap_or(true) {
        roots.push(installed);
    }
    roots
}

/// Directories scanned for legacy packages on the live path (spec 14.3).
///
/// Read from `CRIKEY_LEGACY_PACKAGE_ROOTS` using the platform path-list syntax,
/// so an operator can point `crikey run` at their installed packages today.
/// Resolving roots from the settings file (spec 14.7) is left to a later
/// milestone; an unset variable means no legacy roots, which loads nothing
/// rather than failing.
pub(crate) fn legacy_package_roots() -> Vec<std::path::PathBuf> {
    configured_plugin_roots("CRIKEY_LEGACY_PACKAGE_ROOTS")
}

/// Directories scanned for modern python plugins on the live path (spec 15.1).
///
/// Read from `CRIKEY_MODERN_PLUGIN_ROOTS` using the platform path-list syntax,
/// mirroring [`legacy_package_roots`]. Each root holds `<id>/crikey.toml`
/// plugin subdirectories (contract §11). An unset variable means no modern
/// roots, which loads nothing rather than failing — `crikey run` is unchanged
/// on a host with none.
pub(crate) fn modern_plugin_roots() -> Vec<std::path::PathBuf> {
    configured_plugin_roots("CRIKEY_MODERN_PLUGIN_ROOTS")
}

/// Directories scanned for native packages on the live path (spec 16.1).
///
/// The native provider performs manifest and platform/architecture filtering;
/// this helper only applies the platform path-list syntax and keeps an unset
/// variable equivalent to an empty discovery set.
pub(crate) fn native_plugin_roots() -> Vec<std::path::PathBuf> {
    configured_plugin_roots("CRIKEY_NATIVE_PLUGIN_ROOTS")
}

/// The offline modern package index root, read from `CRIKEY_MODERN_INDEX_ROOT`
/// (the same path-var discipline as [`modern_plugin_roots`]).
///
/// Unset means NO index: declared dependencies do not resolve, and such plugins
/// are recorded unavailable with a clear reason, rather than the launcher
/// trusting a shared, world-writable directory as the hash-verification trust
/// root (spec 15.4). It never defaults to a temporary directory.
fn modern_index_root() -> Option<std::path::PathBuf> {
    std::env::var_os("CRIKEY_MODERN_INDEX_ROOT")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

/// Where the startup-recovery journal lives, read from
/// `CRIKEY_STARTUP_JOURNAL`, else a per-user state directory
/// (`$XDG_STATE_HOME/crikey`, else `$HOME/.local/state/crikey`).
///
/// `None` when no per-user location can be determined. The journal decides
/// whether third-party plugins load at all, so a shared temporary directory is
/// not an acceptable fallback: any local user could plant a record there and
/// either force this account into safe mode or hide a real crash loop from it.
/// Losing recovery for the launch is the smaller failure, and it is announced.
fn startup_journal_path() -> Option<std::path::PathBuf> {
    if let Some(value) = std::env::var_os("CRIKEY_STARTUP_JOURNAL").filter(|value| !value.is_empty()) {
        return Some(std::path::PathBuf::from(value));
    }
    per_user_state_dir().map(|base| base.join("startup.json"))
}

/// Persists the journal, degrading to a diagnostic.
///
/// An unwritable journal costs the next launch its recovery evidence; it must
/// never cost this one its startup.
fn commit_startup_journal(journal: &StartupJournal) {
    if let Err(error) = journal.save() {
        eprintln!("crikey: cannot write the startup-recovery journal: {error}");
    }
}

/// Where the persistent ranking history lives, read from
/// `CRIKEY_SELECTION_HISTORY`, else the same per-user state directory the
/// startup journal uses.
///
/// `None` when no per-user location can be determined, for the same reason the
/// journal refuses one: a shared temporary directory would let any local user
/// plant a record, and this one steers which results the account is shown.
/// Losing history for the launch is the smaller failure, and it is announced.
fn selection_history_path() -> Option<std::path::PathBuf> {
    if let Some(value) = std::env::var_os("CRIKEY_SELECTION_HISTORY").filter(|value| !value.is_empty()) {
        return Some(std::path::PathBuf::from(value));
    }
    per_user_state_dir().map(|base| base.join("selection-history.json"))
}

/// Wall-clock seconds since the Unix epoch, forbidden from going backwards.
///
/// Recency is scored as `now - last_selected`, saturating at zero, so a clock
/// that steps backwards — an NTP correction, a daylight-saving misconfiguration
/// repaired mid-session — would make every recent selection look like it
/// happened in the future and collapse the whole recency term to a flat
/// maximum. Worse, a selection *recorded* under a rewound clock stays wrong on
/// disk for as long as the entry lives. Clamping to the highest value already
/// observed costs a few seconds of frozen recency after a backwards step and
/// keeps the ordering monotone, which is the property the ranker documents.
///
/// A clock the system cannot read at all reads as the epoch, which is the same
/// answer a fresh history already assumes.
#[derive(Debug, Default)]
struct HistoryClock {
    highest: u64,
}

impl HistoryClock {
    /// Samples the system clock and returns the value ranking should use.
    fn advance(&mut self) -> u64 {
        let sampled = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since_epoch| since_epoch.as_secs());
        self.observe(sampled)
    }

    /// The clamp itself, separated from the sample because a test cannot make
    /// the host's clock step backwards and the clamp is the part that has to
    /// be right when it does.
    fn observe(&mut self, sampled: u64) -> u64 {
        self.highest = self.highest.max(sampled);
        self.highest
    }
}

/// The startup-recovery record of the launch in progress (spec 24.2).
///
/// Two parties write it and neither can own it alone: the composition root
/// refreshes the active plugin set as each provider finishes loading, and the
/// renderer callback — which `NativeLauncher::run` takes by value for the rest
/// of the process — records that the launch became usable. Hence one cell both
/// hold, rather than a journal moved into the callback and unreachable
/// afterwards.
///
/// A launcher with no per-user state directory carries no journal; every method
/// here is then a no-op, so the absence is handled once at construction instead
/// of at every call site.
#[derive(Debug)]
struct StartupLedger {
    journal: Option<StartupJournal>,
    ready: bool,
}

impl StartupLedger {
    fn new(journal: Option<StartupJournal>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            journal,
            ready: false,
        }))
    }

    /// Persists the plugins this launch would blame if it died right now.
    ///
    /// Repeats the verdict `begin_startup` already reached; it never charges a
    /// second attempt.
    fn record_active(&mut self, plugins: &[PluginId]) {
        if let Some(journal) = self.journal.as_mut() {
            journal.record_active_plugins(plugins);
            commit_startup_journal(journal);
        }
    }

    /// Clears the failure run, once, on proof that the renderer came up.
    fn mark_renderer_running(&mut self) {
        if self.ready {
            return;
        }
        self.ready = true;
        if let Some(journal) = self.journal.as_mut() {
            journal.mark_ready();
            commit_startup_journal(journal);
        }
    }

    /// Clears the abnormal-shutdown record after a deliberate exit.
    fn mark_clean_shutdown(&mut self) {
        if let Some(journal) = self.journal.as_mut() {
            journal.mark_clean_shutdown();
            commit_startup_journal(journal);
        }
    }
}

/// Wraps the launcher callback so the first event the event loop delivers is
/// what marks this launch ready (spec 24.2).
///
/// `NativeLauncherHandle::request_activation` only QUEUES an event. The window
/// and GPU are created afterwards, from the event loop's `resumed`, and a
/// failure there is terminal: the renderer clears its visible bit and exits,
///
/// which drops the queued activation before any `NativeLauncherEvent` is
/// dispatched. So no event reaching this callback is the observable signature
/// of a renderer that never started, and the first event that does reach it is
/// the earliest proof available to a host that one did.
///
/// Marking ready next to `request_activation` instead would persist zero
/// failures for every launch that dies in renderer startup, and a repeated
/// renderer crash could never reach safe mode — the loop 24.2 exists for.
fn ready_on_first_event<F>(
    ledger: Rc<RefCell<StartupLedger>>,
    mut deliver: F,
) -> impl FnMut(NativeLauncherEvent)
where
    F: FnMut(NativeLauncherEvent) + 'static,
{
    move |event| {
        ledger.borrow_mut().mark_renderer_running();
        deliver(event);
    }
}
/// The persistent search-catalog cache root, read from
/// `CRIKEY_CATALOG_CACHE_ROOT`.
///
/// Unset defaults to a private per-user cache directory, never a shared
/// temporary directory. The cache contains plugin-supplied catalog data and
/// is therefore treated as a trust boundary just like the managed environment
/// and Legacy Compatibility Layer caches.
pub(crate) fn catalog_cache_root() -> Result<std::path::PathBuf, String> {
    if let Some(value) = std::env::var_os("CRIKEY_CATALOG_CACHE_ROOT").filter(|value| !value.is_empty()) {
        let path = std::path::PathBuf::from(value);
        create_private_dir(&path)?;
        return Ok(path);
    }
    let base = per_user_cache_base("the search catalog", "CRIKEY_CATALOG_CACHE_ROOT")?;
    let path = base.join("catalog");
    create_private_dir(&path)?;
    Ok(path)
}

/// The managed-environment cache root, read from `CRIKEY_MODERN_CACHE_ROOT`.
///
/// Unset defaults to a NON-world-writable per-user directory
/// (`$XDG_CACHE_HOME/crikey/modern`, else `$HOME/.cache/crikey/modern`), created
/// `0700` on unix. This is security-critical: [`EnvironmentStore`] reuses a
/// committed env at `cache_root/<env_id>` by PATH with no re-verification of the
/// on-disk bytes, and `<env_id>` is a predictable SHA-256, so a world-writable
/// cache (such as `env::temp_dir()`) would let a local attacker pre-plant
/// `cache_root/<env_id>/site/<evil>.py` that the victim then imports under `-S`.
/// The default is therefore per-user and is NEVER a shared temporary directory;
/// when no per-user location can be determined the launcher refuses rather than
/// fall back to one.
fn modern_cache_root() -> Result<std::path::PathBuf, String> {
    if let Some(value) = std::env::var_os("CRIKEY_MODERN_CACHE_ROOT").filter(|value| !value.is_empty()) {
        let path = std::path::PathBuf::from(value);
        create_private_dir(&path)?;
        return Ok(path);
    }
    let base = per_user_cache_base("modern plugins", "CRIKEY_MODERN_CACHE_ROOT")?;
    let dir = base.join("modern");
    create_private_dir(&dir)?;
    Ok(dir)
}
/// The legacy archive extraction root, read from `CRIKEY_LEGACY_CACHE_ROOT`.
///
/// The loader trusts an existing content-addressed extraction directory, so a
/// shared temporary directory would let another local process plant plugin
/// files before the child interpreter imports them. The default is per-user
/// and restricted to this account, just like the modern environment cache.
pub(crate) fn legacy_cache_root() -> Result<std::path::PathBuf, String> {
    if let Some(value) = std::env::var_os("CRIKEY_LEGACY_CACHE_ROOT").filter(|value| !value.is_empty()) {
        let path = std::path::PathBuf::from(value);
        create_private_dir(&path)?;
        return Ok(path);
    }
    let base = per_user_cache_base("legacy packages", "CRIKEY_LEGACY_CACHE_ROOT")?;
    let path = base.join("legacy");
    create_private_dir(&path)?;
    Ok(path)
}
/// The per-user cache directory CriKey owns, or a message naming what to set.
///
/// [`StandardDirectories`] is the one place that knows a platform's layout:
/// `$XDG_CACHE_HOME/crikey` here, `%LOCALAPPDATA%\CriKey\Cache` on Windows,
/// `~/Library/Caches/CriKey` on macOS. The XDG walk this replaced resolved
/// through `HOME`, which a Windows session started from Explorer does not set,
/// so every cache root below refused and the launcher exited before it could
/// show a window.
fn per_user_cache_base(purpose: &str, override_name: &str) -> Result<std::path::PathBuf, String> {
    StandardDirectories::for_process()
        .map(|directories| directories.cache_dir().to_path_buf())
        .map_err(|error| {
            format!(
                "cannot determine a per-user cache directory for {purpose}: {error} (set \
                 {override_name} or CRIKEY_CACHE_DIR; refusing to use a world-writable shared \
                 temporary directory as a trust root)"
            )
        })
}

/// The per-user state directory CriKey owns, or `None` with the reason left to
/// the caller to announce.
///
/// State is not disposable — it carries the startup-recovery journal and the
/// ranking history — so it has its own platform-resolved location rather than
/// living in the cache.
fn per_user_state_dir() -> Option<std::path::PathBuf> {
    StandardDirectories::for_process()
        .ok()
        .map(|directories| directories.state_dir().to_path_buf())
}

/// Records a fatal startup failure where a desktop launch can be seen to have
/// failed: the per-user `startup.log`, and on Windows a dialog naming it.
///
/// `crikey-launcher` is GUI-subsystem on Windows, so the stderr line the caller
/// has already written is discarded by the operating system. Without this the
/// owner double-clicks a shortcut, nothing appears, and no evidence of why
/// exists anywhere on the machine — indistinguishable from a corrupt download.
/// The dialog names the log because a second failure is diagnosed from the
/// file, not from a box that has already been dismissed.
fn report_desktop_startup_failure(diagnostic: &str) {
    let shown = match append_startup_log(diagnostic) {
        Ok(path) => format!("{diagnostic}\n\nThis was appended to {}.", path.display()),
        // The log is the durable half and the dialog is the visible half.
        // Losing the first must not cost the second, so the dialog carries the
        // reason the log is missing rather than the launch losing both.
        Err(reason) => format!("{diagnostic}\n\nThe startup log could not be written: {reason}"),
    };
    show_startup_failure_dialog(&shown);
}

/// Appends one timestamped line to `startup.log` in the per-user state
/// directory and returns the file it wrote.
///
/// The state directory rather than a location of its own: it is where the
/// startup-recovery journal and the ranking history already live, so a failed
/// launch leaves its evidence beside the state it failed to use, and
/// [`per_user_state_dir`] remains the single place that decides where that is.
/// Appended, never truncated, because a launcher that fails every time is
/// diagnosed from the sequence. The stamp is whole seconds since the Unix
/// epoch, which distinguishes one launch from the next without this crate
/// growing a calendar.
fn append_startup_log(diagnostic: &str) -> Result<std::path::PathBuf, String> {
    use std::io::Write as _;

    let base =
        per_user_state_dir().ok_or_else(|| "no per-user state directory could be resolved".to_owned())?;
    std::fs::create_dir_all(&base).map_err(|error| format!("cannot create `{}`: {error}", base.display()))?;
    let path = base.join("startup.log");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("cannot open `{}`: {error}", path.display()))?;
    writeln!(file, "[{stamp}] {diagnostic}")
        .map_err(|error| format!("cannot write `{}`: {error}", path.display()))?;
    Ok(path)
}

/// Shows `text` in a modal dialog: on Windows the only channel a GUI-subsystem
/// process has before it owns a window.
#[cfg(windows)]
fn show_startup_failure_dialog(text: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TASKMODAL,
    };

    let body = nul_terminated_utf16(text);
    let caption = nul_terminated_utf16("CriKey could not start");
    // SAFETY: both pointers address NUL-terminated UTF-16 buffers that outlive
    // the call. A null owner window is what a process with no window of its own
    // must pass, and `MB_TASKMODAL` with no owner therefore disables this
    // process's windows rather than an unrelated program's.
    #[allow(unsafe_code)]
    let _shown = unsafe {
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_TASKMODAL | MB_SETFOREGROUND,
        )
    };
}

/// Encodes `text` the way `MessageBoxW` demands.
///
/// An interior NUL would end the string there and hide everything after it,
/// which on a path-bearing diagnostic is the half that matters, so it is
/// dropped rather than allowed to truncate the message. Filtering zero code
/// units is exactly that: a surrogate pair never contains one.
#[cfg(windows)]
fn nul_terminated_utf16(text: &str) -> Vec<u16> {
    text.encode_utf16()
        .filter(|unit| *unit != 0)
        .chain(std::iter::once(0))
        .collect()
}

/// No dialog off Windows. A desktop launch there still has an inherited stderr
/// when one was started from a terminal, and the `startup.log` entry covers the
/// launch that had none; standing up a second window system inside the failure
/// path of the first would be a worse thing to debug than the failure.
#[cfg(not(windows))]
fn show_startup_failure_dialog(_text: &str) {}

/// Creates `dir` (and parents) as a private, per-user directory. On unix the
/// leaf is forced to `0700` so a cache later imported by path can never be
/// world- or group-writable. Symlink leaves are rejected: following one would
/// secure a different directory than the configured trust root.
fn create_private_dir(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create private directory `{}`: {error}", dir.display()))?;
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|error| format!("cannot inspect private directory `{}`: {error}", dir.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "private path `{}` is not a real directory",
            dir.display()
        ));
    }
    #[cfg(unix)]
    {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("cannot secure private directory `{}`: {error}", dir.display()))?;
    }
    Ok(())
}

/// The provider group a published row belongs to, used to merge the ranked,
/// legacy, modern and native row sets into one presented frame without one
/// clobbering another.
///
/// `Files` names no row group. The host's file search is ranked into the
/// catalog's own answer rather than appended after it, so its rows arrive as
/// part of the built-in group; what remains source-specific about it is only
/// whether it still owes this generation an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowSource {
    Builtin,
    Files,
    Legacy,
    Modern,
    Native,
}

/// The presented row set kept between UI turns, grouped by source so folding one
/// provider's asynchronous outcome never clobbers another's rows.
///
/// Drivers are submitted without base rows and only wake the host when their
/// own suggestion outcomes arrive. Per-source retention keeps one provider's
/// fold from clobbering another. New generations retire every retained group.
///
/// The ranked group is owned by the UI thread and includes catalog entries of
/// every owner, plus ranked file results. Suggestion groups contain only actual
/// suggestions, never an echoed snapshot of that ranked group.
#[derive(Debug, Default)]
struct RetainedRows {
    generation: Option<Generation>,
    /// The calculator's answer, which leads the frame because when a query
    /// parses as arithmetic the arithmetic is what was asked for. Its own group
    /// so that re-publishing the ranked rows — which is what a file answer
    /// does — cannot drop it.
    calculated: Vec<crikey_ui::ResultRow>,
    commands: Vec<crikey_ui::ResultRow>,
    alias_query: String,
    /// Deliberate Required-input target, not discovery or stored dispatch authority.
    explicit_input: Option<crikey_ui::ResultRow>,
    max_results: Option<usize>,
    /// The ranked answer: catalog items, and the file items merged into them.
    builtin: Vec<crikey_ui::ResultRow>,
    legacy: Vec<crikey_ui::ResultRow>,
    modern: Vec<crikey_ui::ResultRow>,
    native: Vec<crikey_ui::ResultRow>,
    builtin_pending: bool,
    files_pending: bool,
    legacy_pending: bool,
    modern_pending: bool,
    native_pending: bool,
}

impl RetainedRows {
    /// Final presentation follows the complete effective configuration, including
    /// command rows which do not pass through a provider's startup pipeline.
    fn configure(&mut self, store: &ConfigStore) -> bool {
        let limit = Some(pipeline_config_store(Some(store)).limits.max_items_per_query);
        let changed = self.max_results != limit;
        self.max_results = limit;
        changed
    }
    /// Prepares the retained set for `generation`, returning whether it is
    /// current-or-newer (and therefore worth folding). A NEWER generation drops
    /// every stale group first; the SAME generation keeps the groups; an OLDER
    /// (already-superseded) generation is ignored, so a late outcome that
    /// arrives after the UI moved on can never resurrect nor clobber the current
    /// generation's rows.
    fn begin(&mut self, generation: Generation) -> bool {
        match self.generation {
            Some(current) if generation < current => false,
            Some(current) if generation == current => true,
            _ => {
                *self = RetainedRows {
                    generation: Some(generation),
                    max_results: self.max_results,
                    explicit_input: self.explicit_input.take(),
                    ..RetainedRows::default()
                };
                true
            }
        }
    }

    /// The generation the presented rows belong to, which is the generation any
    /// selection made against them has to resolve under.
    fn generation(&self) -> Option<Generation> {
        self.generation
    }

    /// Refreshes the calculator's group, which leads the frame.
    fn set_calculated(&mut self, generation: Generation, rows: Vec<crikey_ui::ResultRow>) {
        if !self.begin(generation) {
            return;
        }
        self.calculated = rows;
    }

    /// Refreshes the ranked group.
    ///
    /// Called twice for one generation whenever a file answer lands: once for
    /// the catalog's own answer and again once the file items have been ranked
    /// into it. Pending is not a parameter for that reason — the second call
    /// says nothing new about who still owes an answer.
    fn set_builtin(&mut self, generation: Generation, rows: Vec<crikey_ui::ResultRow>) {
        if !self.begin(generation) {
            return;
        }
        self.builtin = rows;
    }
    fn set_commands(&mut self, generation: Generation, rows: Vec<crikey_ui::ResultRow>) {
        if self.begin(generation) {
            self.commands = rows;
        }
    }

    /// Records that a provider's answer is still outstanding for `generation`, so
    /// the merged frame stays marked pending until that outcome folds in.
    fn mark_pending(&mut self, generation: Generation, source: RowSource, pending: bool) {
        if !self.begin(generation) {
            return;
        }
        match source {
            RowSource::Builtin => self.builtin_pending = pending,
            RowSource::Files => self.files_pending = pending,
            RowSource::Legacy => self.legacy_pending = pending,
            RowSource::Modern => self.modern_pending = pending,
            RowSource::Native => self.native_pending = pending,
        }
    }

    /// Folds a driver's suggestion-only outcome. The caller identifies the
    /// source by the driver, not by a row's owner or display label.
    fn absorb(
        &mut self,
        generation: Generation,
        source: RowSource,
        rows: &[crikey_ui::ResultRow],
        pending: bool,
    ) {
        if !self.begin(generation) {
            return;
        }
        let own = rows.to_vec();
        match source {
            // Neither the host's own groups nor the file source publish rows
            // through this path; both are refreshed by the UI thread directly.
            RowSource::Builtin | RowSource::Files => {}
            RowSource::Legacy => self.legacy = own,
            RowSource::Modern => self.modern = own,
            RowSource::Native => self.native = own,
        }
        self.mark_pending(generation, source, pending);
    }

    /// Calculated rows lead and supersede every exact-owner/id copy. Ranked
    /// catalog-only rows follow, then suggestions in provider publication order.
    /// An exact-owner/id suggestion supersedes its catalog copy and payload.
    fn project_rows(
        &self,
        search: &SearchService,
        scope: Option<&crikey_ui::ScopeIdentity>,
    ) -> (Vec<crikey_ui::ResultRow>, Vec<crikey_ui::SectionHeader>) {
        // Explicit input is not discovery: its captured target survives a zero
        // result cap until the user's next actual query/selection replacement.
        let limit = if self.explicit_input.is_some() {
            None
        } else {
            self.max_results
        };
        search.project_root_rows(self.merge_rows(), scope, limit)
    }
    fn merge_rows(&self) -> Vec<crikey_ui::ResultRow> {
        if let Some(row) = &self.explicit_input {
            return vec![row.clone()];
        }
        let mut rows = Vec::with_capacity(
            self.calculated.len()
                + self.builtin.len()
                + self.legacy.len()
                + self.modern.len()
                + self.native.len(),
        );
        let mut commands = std::collections::BTreeSet::new();
        rows.extend(
            self.commands
                .iter()
                .filter(|row| {
                    !self.alias_query.is_empty()
                        && row.command_state.as_ref().is_some_and(|state| {
                            state
                                .aliases
                                .iter()
                                .any(|alias| alias.to_lowercase().starts_with(&self.alias_query))
                        })
                })
                .filter(|row| match &row.target {
                    crikey_ui::ActionTarget::Command(reference) => commands.insert(reference.clone()),
                    _ => false,
                })
                .cloned(),
        );
        rows.extend(self.calculated.iter().cloned());
        rows.extend(
            self.builtin
                .iter()
                .filter(|catalog| {
                    !self
                        .legacy
                        .iter()
                        .chain(&self.modern)
                        .chain(&self.native)
                        .chain(&self.calculated)
                        .any(|suggestion| suggestion.target == catalog.target)
                })
                .cloned(),
        );
        rows.extend(
            self.legacy
                .iter()
                .chain(&self.modern)
                .chain(&self.native)
                .filter(|suggestion| {
                    !self
                        .calculated
                        .iter()
                        .any(|calculated| calculated.target == suggestion.target)
                })
                .cloned(),
        );
        rows.extend(
            self.commands
                .iter()
                .filter(|row| match &row.target {
                    crikey_ui::ActionTarget::Command(reference) => commands.insert(reference.clone()),
                    crikey_ui::ActionTarget::Result { .. } => false,
                })
                .cloned(),
        );
        rows
    }

    /// Whether any source still has work outstanding for the retained generation.
    fn pending(&self) -> bool {
        self.builtin_pending
            || self.files_pending
            || self.legacy_pending
            || self.modern_pending
            || self.native_pending
    }
}

/// Ranks one file-search outcome into the answer and refreshes the retained
/// rows from it.
///
/// This is where the launcher stopped concatenating. The file items go through
/// the same matcher, the same history signals and the same ranker as the
/// catalog's, so a file whose name the query is a prefix of outranks an
/// application the query merely appears inside — which no amount of appending
/// after the catalog could ever produce. Republishing the whole ranked group is
/// the point: the file rows are *in* it, at whatever position they earned.
///
/// The pending mark comes off either way. A generation the merge refuses is one
/// the retained rows have already moved past, and `mark_pending` ignores it.
fn absorb_file_items(
    search: &mut SearchService,
    retained: &mut RetainedRows,
    owner: &PluginId,
    outcome: file_provider::FileOutcome,
) {
    if search.merge_query_items(outcome.generation, owner, outcome.items) {
        retained.set_builtin(outcome.generation, search.result_rows());
    }
    retained.mark_pending(outcome.generation, RowSource::Files, outcome.pending);
}

fn application_provider_policy() -> PluginPolicy {
    PluginPolicy {
        profile: SchedulingProfile::Modern,
        debounce: DebouncePolicy {
            debounce_ms: 0,
            maximum_wait_ms: None,
            leading_edge: true,
            trailing_edge: true,
            minimum_query_length: 0,
        },
        activation: ActivationPolicy {
            supports_empty_query: true,
            prefixes: Vec::new(),
            keywords: Vec::new(),
            patterns: Vec::new(),
        },
        max_concurrent_requests: 1,
        queue_policy: QueuePolicy::ReplaceOldest,
        queue_capacity: 1,
    }
}

/// One admission edge for ordinary queries and deliberately opened command input.
/// Never advance SearchService without advancing its catalog pipeline as well.
fn admit_root_query(
    search: &mut SearchService,
    pipeline: &mut QueryPipeline,
    owner: &PluginId,
    model: &mut LauncherViewModel,
    retained: &mut RetainedRows,
    query: &str,
    now: u64,
) -> Result<(Generation, Result<(), String>), String> {
    let generation = search.submit_query(query).map_err(|error| error.to_string())?;
    model.begin_generation(generation);
    retained.begin(generation);
    let catalog = if let Some(scope) = model.selected_scope() {
        let scoped_generation =
            pipeline.keystroke_scoped(query, now, Some(crikey_app::root_scope_from_identity(scope)));
        let tick = pipeline.tick(now);
        for cancellation in tick.cancellations {
            let _ = pipeline.complete(&cancellation.plugin, cancellation.generation, now);
        }
        retained.set_builtin(generation, search.result_rows());
        retained.mark_pending(generation, RowSource::Builtin, false);
        if scoped_generation == generation {
            Ok(())
        } else {
            Err("pipeline and search generations differ".into())
        }
    } else {
        prepare_catalog_rows(search, pipeline, owner, query, generation, now, retained)
    };
    Ok((generation, catalog))
}

/// The ranked catalog includes plugin-owned items. Only the host-owned subset
/// passes through the builtin scheduler; presentation keeps the complete ranked
/// answer and its original owners, highlights and shared row payloads.
fn prepare_catalog_rows(
    search: &SearchService,
    pipeline: &mut QueryPipeline,
    owner: &PluginId,
    query: &str,
    generation: Generation,
    now: u64,
    retained: &mut RetainedRows,
) -> Result<(), String> {
    let items = search
        .results()
        .iter()
        .filter(|hit| hit.item.plugin_id == *owner)
        // This synchronous producer already holds ranked results. Respect both
        // aggregate quotas before copying/enqueuing, not after boundary refusal.
        .take(
            pipeline
                .max_items_per_query()
                .min(pipeline.max_items_per_plugin_per_query()),
        )
        .map(|hit| hit.item.clone())
        .collect();
    let result = drive_application_provider(pipeline, owner, query, items, now).and_then(|frame| {
        if frame.generation != generation {
            Err("pipeline and search generations differ".to_owned())
        } else {
            Ok(frame)
        }
    });
    retained.set_builtin(generation, search.result_rows());
    // A failed synchronous request cannot still owe an asynchronous answer.
    retained.mark_pending(
        generation,
        RowSource::Builtin,
        result.as_ref().is_ok_and(|frame| frame.pending_plugins),
    );
    result.map(|_| ())
}

fn drive_application_provider(
    pipeline: &mut QueryPipeline,
    owner: &PluginId,
    query: &str,
    items: Vec<Item>,
    now: u64,
) -> Result<ViewModel, String> {
    let mut items = items;
    items.truncate(
        pipeline
            .max_items_per_query()
            .min(pipeline.max_items_per_plugin_per_query()),
    );
    let generation = pipeline.keystroke(query, now);
    let tick = pipeline.tick(now);
    let mut errors: Vec<String> = tick.errors.iter().map(|error| format!("{error:?}")).collect();

    for cancellation in tick.cancellations {
        let _ = pipeline.complete(&cancellation.plugin, cancellation.generation, now);
    }

    let mut items = Some(items);
    let mut delivered_current = false;
    for request in tick.dispatches {
        if request.plugin != *owner {
            continue;
        }
        if request.generation != generation {
            let _ = pipeline.complete(&request.plugin, request.generation, now);
            continue;
        }
        let Some(batch_items) = items.take() else {
            let _ = pipeline.complete(&request.plugin, request.generation, now);
            continue;
        };
        // Split to the aggregator's ceiling. A batch above it is refused
        // whole -- not truncated -- and a refused batch leaves the request
        // unsettled, so the frame never presents and the launcher keeps
        // saying "Providers are still responding" until the next keystroke.
        // Selection admits up to `max_items_per_plugin_per_query` (250) while
        // one batch may carry `max_items_per_batch` (50), so any common letter
        // typed against a real Start Menu exceeded it.
        let ceiling = pipeline.max_items_per_batch().max(1);
        let batch_count = batch_items.len().div_ceil(ceiling).max(1);
        delivered_current = true;
        for (index, chunk) in batch_items.chunks(ceiling).enumerate() {
            // Only the last chunk ends the stream: a `Final` in the middle
            // would terminate it and every later chunk would be refused as
            // arriving after the end.
            let state = if index + 1 == batch_count {
                BatchState::Final
            } else {
                BatchState::Partial
            };
            if let Err(error) = pipeline.deliver(
                ResultBatch {
                    generation: request.generation,
                    plugin: request.plugin.clone(),
                    state,
                    items: chunk.to_vec(),
                },
                now,
            ) {
                errors.push(format!("{error:?}"));
                delivered_current = false;
            }
        }
        // An empty result set still owes the pipeline a terminal batch, or the
        // request stays outstanding exactly as an over-large one did.
        if batch_items.is_empty() {
            if let Err(error) = pipeline.deliver(
                ResultBatch {
                    generation: request.generation,
                    plugin: request.plugin.clone(),
                    state: BatchState::Final,
                    items: Vec::new(),
                },
                now,
            ) {
                errors.push(format!("{error:?}"));
                delivered_current = false;
            }
        }
        let _ = pipeline.complete(&request.plugin, request.generation, now);
    }

    // Drained, not merely presented: every chunk delivered above is already in
    // hand, and leaving any of it queued leaves the request unsettled.
    let frame = pipeline.present_drained(now);
    errors.extend(pipeline.take_errors().iter().map(|error| format!("{error:?}")));
    if !delivered_current || !errors.is_empty() {
        pipeline.abort_request(owner, generation, now);
        return Err(format!(
            "synchronous request did not settle: {}",
            errors.join("; ")
        ));
    }
    frame
        .filter(|frame| frame.generation == generation && !frame.pending_plugins)
        .ok_or_else(|| {
            pipeline.abort_request(owner, generation, now);
            "synchronous request produced no settled current frame".to_owned()
        })
}

/// Routes a `crikey dev` invocation.
fn dev(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") if args.len() == 1 => {
            print!("{DEV_USAGE}");
            ExitCode::SUCCESS
        }
        Some("benchmark") => benchmark(&args[1..]),
        Some("run") => modern_commands::run(&args[1..]),
        Some("test") => modern_commands::test(&args[1..]),
        Some("trace-query") => dev_commands::trace_query(&args[1..]),
        Some("simulate-typing") => dev_commands::simulate_typing(&args[1..]),
        Some("test-legacy-compat") => legacy_commands::test_legacy_compat(&args[1..]),
        Some("inspect-catalog") => legacy_commands::inspect_catalog(&args[1..]),
        Some("compatibility-report") => legacy_commands::compatibility_report(&args[1..]),
        Some("inspect-protocol") => native_commands::inspect_protocol(&args[1..]),
        Some("measure-activation") => activation_commands::measure_activation(&args[1..]),
        Some(other) => {
            eprintln!("crikey: unknown dev subcommand `{other}`\n\n{DEV_USAGE}");
            ExitCode::from(64) // EX_USAGE
        }
        None => {
            eprintln!("crikey: `dev` needs a subcommand\n\n{DEV_USAGE}");
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
    let mut help = false;
    let mut remaining = args.iter();

    while let Some(arg) = remaining.next() {
        let value = match arg.as_str() {
            "-h" | "--help" => {
                help = true;
                continue;
            }
            "--items" => remaining.next().ok_or("`--items` needs a value")?.as_str(),
            other => other
                .strip_prefix("--items=")
                .ok_or_else(|| format!("unrecognized `dev benchmark` argument `{other}`"))?,
        };
        items = value
            .parse::<usize>()
            .map_err(|_| format!("`--items` needs a whole number of items, got `{value}`"))?;
    }

    if help {
        return Ok(Request::Usage);
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

fn publish_learning(
    learning: command_history::Learning,
    search: &mut SearchService,
    writer: &command_history::HistoryWorker,
    reset_pending: bool,
    deferred: &mut Vec<command_history::Learning>,
) -> Result<(), String> {
    if reset_pending {
        deferred.push(learning);
        return Ok(());
    }
    learning.publish(search).map_err(str::to_owned)?;
    writer.save(search.selection_history_snapshot())
}

fn compose_fallback_search(
    query: &str,
    view: crikey_ui::ViewId,
    generation: Generation,
    catalog: crikey_app::OrdinaryProviderState,
    summaries: &[Option<crikey_app::OrdinarySearchSummary>],
) -> Option<crikey_app::FallbackSearch> {
    let mut states = vec![catalog];
    for summary in summaries {
        match summary {
            Some(summary) if summary.view == view.get() && summary.generation == generation => {
                states.extend(summary.providers.iter().map(|provider| provider.state));
            }
            _ => states.push(crikey_app::OrdinaryProviderState::Pending),
        }
    }
    let mut search = crikey_app::FallbackSearch::new(query.to_owned(), states.len()).ok()?;
    for (index, state) in states.into_iter().enumerate() {
        search.update(index, state).ok()?;
    }
    Some(search)
}

fn publish_root_rows(
    search: &SearchService,
    configuration: Option<&ConfigStore>,
    retained: &mut RetainedRows,
    model: &mut LauncherViewModel,
    generation: Generation,
    query: &str,
    fallback: Option<&crikey_app::FallbackSearch>,
) {
    if let Some(target) = retained.explicit_input.as_ref().map(|row| row.target.clone()) {
        // Refresh presentation from the registry, but never run discovery or cap
        // an explicitly requested input target (including hidden declarations).
        retained.explicit_input = search
            .command_management_rows()
            .into_iter()
            .find(|row| row.target == target);
    }
    let home = configuration
        .and_then(|store| store.get("launcher.command-home"))
        .unwrap_or("compact");
    let mut context = crikey_app::CommandCompositionContext::root(generation, query);
    context.compact = home == "compact";
    context.include_favorites = matches!(home, "favorites" | "favorites-and-recent");
    context.include_recent = matches!(home, "recent" | "favorites-and-recent");
    context.fallback = if model.selected_scope().is_none() {
        fallback
    } else {
        None
    };
    retained.set_commands(generation, search.compose_command_rows(context));
    retained.alias_query = query.trim().to_lowercase();
    publish_projected_rows(search, retained, model, generation);
}

fn publish_projected_rows(
    search: &SearchService,
    retained: &RetainedRows,
    model: &mut LauncherViewModel,
    generation: Generation,
) {
    if !model.admits_root_generation(generation) || retained.generation() != Some(generation) {
        return;
    }
    let mut choices = search.root_scope_choices();
    let scope = model.selected_scope().cloned();
    if let Some(scope) = &scope {
        if search.resolve_root_scope(scope).is_none() {
            model.set_navigation_notice(
                "Selected scope is unavailable. Choose All or another scope explicitly.".into(),
            );
        } else {
            // A dynamic declaration lease survives executable-generation retirement.
            // Fresh memberships may advance generation, never owner or producer.
            for choice in &mut choices {
                if choice.identity.owner == scope.owner
                    && choice.identity.id == scope.id
                    && matches!((&scope.source, &choice.identity.source),
                        (crikey_ui::PublicationIdentity::Suggestion { producer: selected, .. },
                         crikey_ui::PublicationIdentity::Suggestion { producer: current, .. }) if selected == current)
                {
                    choice.identity = scope.clone();
                }
            }
            if !choices.iter().any(|choice| &choice.identity == scope) {
                let label = model.selected_scope_choice().map_or_else(
                    || format!("{} — {}", scope.owner.0, scope.id.0),
                    |choice| choice.label.clone(),
                );
                choices.push(crikey_ui::ScopeChoice {
                    identity: scope.clone(),
                    label,
                });
            }
        }
    }
    let (rows, headers) = retained.project_rows(search, scope.as_ref());
    let actionless = rows
        .iter()
        .filter_map(|row| {
            let crikey_ui::ActionTarget::Result { owner, item } = &row.target else {
                return None;
            };
            (row.default_action.is_none()
                && row.alternate_actions.is_empty()
                && search
                    .root_result_binding(owner, item, None, row.source.as_ref())
                    .is_some())
            .then(|| crikey_core::structured_arguments::OwnedItem {
                owner: owner.clone(),
                item: item.clone(),
            })
        })
        .collect();
    model.set_root_presentation(generation, choices, headers, actionless);
    model.publish(generation, rows, retained.pending());
}

/// Apply the current native intent after draining queued shortcuts. A newly
/// admitted shortcut cannot borrow authority from before this command executes.
fn apply_native_command(
    model: &mut LauncherViewModel,
    retained: &mut RetainedRows,
    hotkey_intent: &mut u64,
    command: crikey_ui::UiCommand,
) -> Option<UiEffect> {
    *hotkey_intent = hotkey_intent.checked_add(1).expect("shortcut intent exhausted");
    if matches!(
        &command,
        crikey_ui::UiCommand::SetQuery(_)
            | crikey_ui::UiCommand::Cancel
            | crikey_ui::UiCommand::Back
            | crikey_ui::UiCommand::Dismiss
            | crikey_ui::UiCommand::OpenSettings
            | crikey_ui::UiCommand::CloseSettings
    ) {
        retained.explicit_input = None;
    }
    model.apply(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_core::ItemId;
    #[cfg(target_os = "linux")]
    use crikey_plugin_model::Permissions;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// A runtime that accepts every page and counts the closes, so a test can
    /// tell "never opened" apart from "opened and rolled back".
    #[derive(Default)]
    struct PageRuntime {
        opens: std::sync::Mutex<u32>,
        closes: std::sync::Mutex<u32>,
    }

    impl crikey_app::PluginActionExecutor for PageRuntime {
        fn submit_plugin_action(
            &self,
            _plugin: &PluginId,
            _item: &crikey_core::Item,
            _action_id: &crikey_core::ActionId,
            _argument: Option<&str>,
        ) -> crikey_core::Result<crikey_app::ActionRequestId> {
            Err(crikey_core::CoreError::Invalid("not used".to_owned()))
        }

        fn open_plugin_page(
            &self,
            _plugin: &PluginId,
            _page_id: &str,
            _width: u32,
            _height: u32,
            _palette: crikey_core::PagePalette,
        ) -> crikey_core::Result<()> {
            *self.opens.lock().expect("stub state") += 1;
            Ok(())
        }

        fn close_plugin_page(&self) {
            *self.closes.lock().expect("stub state") += 1;
        }
    }

    #[test]
    fn settings_back_reopens_a_fresh_page_session_without_replaying_execution() {
        let runtime = Arc::new(PageRuntime::default());
        let mut search = page_service(&runtime);
        let mut model = LauncherViewModel::new();
        let mut web = web_page_runtime();
        let owner = PluginId("test.page".into());
        model.activate();
        open_plugin_page(&mut search, &mut model, &mut web, &owner, "nested", (800, 600)).unwrap();
        let old_view = model.view();
        assert_eq!(
            model.apply(crikey_ui::UiCommand::OpenSettings),
            Some(UiEffect::ClosePage)
        );
        web.close("covered by settings");
        search.close_page();
        assert!(search.page_owner().is_none());
        assert_eq!(*runtime.closes.lock().unwrap(), 1);
        model.apply(crikey_ui::UiCommand::CloseSettings);
        let crikey_ui::NavigationRoute::CustomPage { plugin, page_id, .. } =
            model.take_page_reopen().unwrap()
        else {
            panic!("expected custom route");
        };
        open_plugin_page(&mut search, &mut model, &mut web, &plugin, &page_id, (800, 600)).unwrap();
        assert_ne!(model.view(), old_view);
        assert_eq!(*runtime.opens.lock().unwrap(), 2);
        assert_eq!(search.page_owner(), Some((owner, "nested".into())));
        assert!(model.page().unwrap().web_surface_id.is_none());
        assert!(!model.page().unwrap().answered);
    }

    #[test]
    fn vanished_suspended_page_owner_has_visible_back_instead_of_a_loading_surface() {
        let runtime = Arc::new(PageRuntime::default());
        let mut search = page_service(&runtime);
        let mut model = LauncherViewModel::new();
        let mut web = web_page_runtime();
        model.activate();
        open_plugin_page(
            &mut search,
            &mut model,
            &mut web,
            &PluginId("test.page".into()),
            "nested",
            (800, 600),
        )
        .unwrap();
        model.apply(crikey_ui::UiCommand::OpenSettings);
        search.close_page();
        model.close_settings();
        let crikey_ui::NavigationRoute::CustomPage { plugin, page_id, .. } =
            model.take_page_reopen().unwrap()
        else {
            panic!("expected custom route");
        };
        let mut unavailable = SearchService::new(App::new());
        assert!(open_plugin_page(
            &mut unavailable,
            &mut model,
            &mut web,
            &plugin,
            &page_id,
            (800, 600)
        )
        .is_err());
        let frame = model.frame().unwrap();
        assert!(frame.page.is_none());
        assert!(frame.navigation.unavailable.is_some() && frame.navigation.can_go_back);
        model.apply(crikey_ui::UiCommand::Back);
        assert_eq!(model.view().unwrap().1, crikey_ui::ViewKind::RootList);
    }

    fn page_service(runtime: &Arc<PageRuntime>) -> SearchService {
        page_service_with_permissions(
            runtime,
            crikey_plugin_model::Permissions::legacy_compatibility_baseline(),
        )
    }

    fn page_service_with_permissions(
        runtime: &Arc<PageRuntime>,
        permissions: crikey_plugin_model::Permissions,
    ) -> SearchService {
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions(
                [(PluginId("test.page".to_owned()), permissions)],
                Arc::clone(runtime) as Arc<dyn crikey_app::PluginActionExecutor>,
            )
            .expect("the stub registers once");
        let mut service = SearchService::new(App::new());
        service.set_plugin_action_router(Arc::new(router));
        service
    }
    fn web_page_runtime() -> WebPageRuntime {
        #[cfg(target_os = "linux")]
        {
            WebPageRuntime::new(std::path::PathBuf::from("/unused"), Arc::new(|| {}), (1, 1))
        }
        #[cfg(not(target_os = "linux"))]
        {
            WebPageRuntime::new()
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn denied_web_declarations_never_construct_a_supervisor() {
        for (permissions, declaration, owner, reason) in [
            (
                Permissions::default(),
                PageWebSurface {
                    url: "https://example.test".to_owned(),
                    storage: WebStorage::Ephemeral,
                },
                PluginId("test.page".to_owned()),
                "network",
            ),
            (
                Permissions {
                    network: true,
                    ..Permissions::default()
                },
                PageWebSurface {
                    url: "https://example.test".to_owned(),
                    storage: WebStorage::Persistent,
                },
                PluginId("test.page".to_owned()),
                "web-storage",
            ),
            (
                Permissions {
                    network: true,
                    web_storage: true,
                    ..Permissions::default()
                },
                PageWebSurface {
                    url: "https://example.test".to_owned(),
                    storage: WebStorage::Persistent,
                },
                PluginId("native.test.page".to_owned()),
                "no registered permissions",
            ),
        ] {
            let plugin_runtime = Arc::new(PageRuntime::default());
            let mut search = page_service_with_permissions(&plugin_runtime, permissions);
            let mut view_model = LauncherViewModel::default();
            let mut web_page = web_page_runtime();
            view_model.activate();
            open_plugin_page(
                &mut search,
                &mut view_model,
                &mut web_page,
                &PluginId("test.page".to_owned()),
                "page",
                (800, 600),
            )
            .expect("page opens");
            assert!(view_model.update_page(
                Arc::new(crikey_core::PageFrame {
                    generation: 1,
                    web: Some(declaration.clone()),
                    ..crikey_core::PageFrame::default()
                }),
                false
            ));
            let error = web_page
                .declare(&search, &mut view_model, &owner, &declaration)
                .expect_err("exact owner's manifest refuses the request");
            assert!(error.contains(reason), "{error}");
            assert!(
                web_page.supervisor.is_none(),
                "denial precedes manager construction"
            );
            assert!(web_page.active.is_none(), "no denied surface becomes active");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stay_without_a_document_never_constructs_a_supervisor() {
        let plugin_runtime = Arc::new(PageRuntime::default());
        let mut search = page_service_with_permissions(&plugin_runtime, Permissions::default());
        let mut view_model = LauncherViewModel::default();
        let mut web_page = web_page_runtime();
        view_model.activate();
        open_plugin_page(
            &mut search,
            &mut view_model,
            &mut web_page,
            &PluginId("test.page".to_owned()),
            "page",
            (800, 600),
        )
        .expect("page opens");
        let declaration = PageWebSurface {
            url: String::new(),
            storage: WebStorage::Ephemeral,
        };
        assert!(view_model.update_page(
            Arc::new(crikey_core::PageFrame {
                generation: 1,
                web: Some(declaration.clone()),
                ..crikey_core::PageFrame::default()
            }),
            false
        ));
        let error = web_page
            .declare(
                &search,
                &mut view_model,
                &PluginId("test.page".to_owned()),
                &declaration,
            )
            .expect_err("stay cannot create a document");
        assert!(error.contains("first web-surface declaration"));
        assert!(web_page.supervisor.is_none());
        assert!(web_page.active.is_none());
    }

    /// A late action completion cannot create a surface or a hidden session.
    #[test]
    fn a_page_opening_into_a_dismissed_launcher_is_not_admitted() {
        let runtime = Arc::new(PageRuntime::default());
        let mut search = page_service(&runtime);
        let mut view_model = LauncherViewModel::default();
        let mut web_page = web_page_runtime();
        view_model.dismiss();
        assert!(!view_model.is_visible(), "the launcher starts dismissed");

        open_plugin_page(
            &mut search,
            &mut view_model,
            &mut web_page,
            &PluginId("test.page".to_owned()),
            "page",
            (800, 600),
        )
        .expect("a dismissed launcher is not a failure");

        assert!(
            search.page_owner().is_none(),
            "no page owns a screen the user cannot see"
        );
        assert!(view_model.page().is_none(), "the model took no surface");
    }

    /// The rollback must not fire on the ordinary path, or no page could ever
    /// open.
    #[test]
    fn a_page_opening_into_a_visible_launcher_survives() {
        let runtime = Arc::new(PageRuntime::default());
        let mut search = page_service(&runtime);
        let mut view_model = LauncherViewModel::default();
        let mut web_page = web_page_runtime();
        view_model.activate();

        open_plugin_page(
            &mut search,
            &mut view_model,
            &mut web_page,
            &PluginId("test.page".to_owned()),
            "page",
            (800, 600),
        )
        .expect("the page opens");

        assert_eq!(
            *runtime.closes.lock().expect("stub state"),
            0,
            "an accepted page is not closed behind the user's back"
        );
        assert_eq!(
            search.page_owner(),
            Some((PluginId("test.page".to_owned()), "page".to_owned())),
            "the page owns the screen"
        );
        assert!(view_model.page().is_some(), "the model shows the surface");
    }

    #[test]
    fn failed_page_diagnostic_reaches_the_first_refreshed_root_frame() {
        let runtime = Arc::new(PageRuntime::default());
        let mut search = page_service(&runtime);
        for stage in [
            StartupStage::WindowAndHotkey,
            StartupStage::PersistedCatalog,
            StartupStage::AcceptQueries,
        ] {
            search.complete_stage(stage).expect("query startup completes");
        }
        let owner = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
        search
            .replace_catalog(&owner, 1, vec![application("Alpha old")])
            .expect("the initial catalog is valid");
        let mut view_model = LauncherViewModel::default();
        let mut web_page = web_page_runtime();
        view_model.activate();
        view_model.apply(crikey_ui::UiCommand::SetQuery("Alpha".to_owned()));
        let old = search.submit_query("Alpha").expect("the initial query succeeds");
        view_model.begin_generation(old);
        view_model.publish(old, search.result_rows(), false);
        open_plugin_page(
            &mut search,
            &mut view_model,
            &mut web_page,
            &PluginId("test.page".to_owned()),
            "page",
            (800, 600),
        )
        .expect("the page opens");
        let _ = view_model.frame();
        let refreshed_item = application("Alpha fresh");
        let refreshed_id = refreshed_item.stable_id.clone();
        search
            .replace_catalog(&owner, 2, vec![refreshed_item])
            .expect("the catalog changes while the page covers the root");

        let message = close_failed_web_page(
            &mut web_page,
            &mut search,
            &mut view_model,
            "input transport failed".to_owned(),
        );
        assert!(search.page_owner().is_none());
        assert!(view_model.page().is_none());
        assert_eq!(*runtime.closes.lock().expect("runtime state"), 1);

        // Match the callback's refresh drain without consuming a graphical
        // frame between the failure and the fresh synchronous publication.
        let raw = view_model
            .take_query_refresh()
            .expect("closing requests a search");
        assert_eq!(raw, "Alpha");
        let fresh = search.submit_query(&raw).expect("the return query succeeds");
        assert!(fresh > old);
        view_model.begin_generation(fresh);
        view_model.publish(fresh, search.result_rows(), false);
        assert_eq!(view_model.take_query_refresh(), None);
        report_status(&mut view_model, message);

        let frame = view_model.frame().expect("the next graphical frame is ready");
        assert_eq!(frame.generation, fresh);
        assert_eq!(frame.rows.len(), 1);
        assert_eq!(result_identity(&frame.rows[frame.selected]).1, &refreshed_id);
        assert_eq!(
            frame.rows[frame.selected].status.as_deref(),
            Some("Page closed: input transport failed")
        );
    }

    #[test]
    fn reactivation_closes_a_page_even_when_dismissal_was_superseded() {
        let runtime = Arc::new(PageRuntime::default());
        let mut search = page_service(&runtime);
        let mut view_model = LauncherViewModel::default();
        let mut web_page = web_page_runtime();
        let plugin = PluginId("test.page".to_owned());
        view_model.activate();
        open_plugin_page(
            &mut search,
            &mut view_model,
            &mut web_page,
            &plugin,
            "page",
            (800, 600),
        )
        .expect("the old session opens a page");

        activate_launcher_session(&mut search, &mut view_model, &mut web_page);

        assert!(view_model.is_visible(), "the new launcher session is visible");
        assert!(view_model.page().is_none(), "the new session has no old surface");
        assert!(
            search.page_owner().is_none(),
            "the old plugin no longer owns a page"
        );
        assert_eq!(
            *runtime.closes.lock().expect("runtime state"),
            1,
            "the plugin is told its old page closed even without a dismissal event",
        );

        activate_launcher_session(&mut search, &mut view_model, &mut web_page);
        assert_eq!(
            *runtime.closes.lock().expect("runtime state"),
            1,
            "repeated activation does not close an already retired page twice",
        );

        open_plugin_page(
            &mut search,
            &mut view_model,
            &mut web_page,
            &plugin,
            "replacement",
            (800, 600),
        )
        .expect("the replacement session can open its own page");
        assert_eq!(
            search.page_owner(),
            Some((plugin, "replacement".to_owned())),
            "only the new page owns the replacement session",
        );
        assert_eq!(*runtime.opens.lock().expect("runtime state"), 2);
        assert_eq!(*runtime.closes.lock().expect("runtime state"), 1);
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
    fn top_level_help_rejects_unknown_extra_arguments() {
        assert_ne!(dispatch(&args(&["help", "--unknown"])), ExitCode::SUCCESS);
        assert_ne!(dispatch(&args(&["version", "--unknown"])), ExitCode::SUCCESS);
        assert_ne!(dispatch(&args(&["--help", "--unknown"])), ExitCode::SUCCESS);
    }

    #[test]
    fn command_help_is_available_without_starting_work() {
        assert_eq!(dispatch(&args(&["run", "--help"])), ExitCode::SUCCESS);
        assert_eq!(dispatch(&args(&["dev", "--help"])), ExitCode::SUCCESS);
        assert_eq!(dispatch(&args(&["package", "--help"])), ExitCode::SUCCESS);
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
        assert!(
            parse_benchmark_args(&args(&["--help", "--unknown"])).is_err(),
            "help must not hide an unknown benchmark option"
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
    fn built_in_application_results_cross_intake_before_prompt_publication() {
        use std::collections::BTreeMap;

        use crikey_core::{ArgumentPolicy, Category, HitPolicy, ItemId};
        use crikey_input_scheduler::{BatchCompletion, QueryTraceEvent};

        let owner = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
        let application = |id: &str, label: &str, score_hint: i32| Item {
            stable_id: ItemId(id.to_owned()),
            plugin_id: owner.clone(),
            category: Category::Application,
            label: label.to_owned(),
            description: format!("launch {label}"),
            target: format!("app://{id}"),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        };

        let mut search = SearchService::new(App::new());
        search
            .complete_stage(StartupStage::WindowAndHotkey)
            .expect("window startup completes");
        search
            .replace_catalog(
                &owner,
                1,
                vec![
                    application("firefox", "Firefox Browser", 30),
                    application("campfire", "Campfire Notes", 20),
                    application("water", "Water Clock", 10),
                ],
            )
            .expect("the application catalog is valid");
        search
            .complete_stage(StartupStage::PersistedCatalog)
            .expect("catalog startup completes");
        search
            .complete_stage(StartupStage::AcceptQueries)
            .expect("query startup completes");

        let search_generation = search.submit_query("fire").expect("the local query is immediate");
        let search_rows = search.result_rows();
        assert_eq!(search_rows.len(), 2);
        assert!(
            search_rows.iter().all(|row| !row.highlights.is_empty()),
            "the renderer must receive SearchService match evidence"
        );
        let expected_ids = search_rows
            .iter()
            .map(|row| result_identity(row).1.clone())
            .collect::<Vec<_>>();
        let expected_highlights = search_rows
            .iter()
            .map(|row| row.highlights.clone())
            .collect::<Vec<_>>();
        let result_items = search
            .results()
            .iter()
            .map(|hit| hit.item.clone())
            .collect::<Vec<_>>();

        let mut pipeline = QueryPipeline::new(PipelineConfig::default());
        pipeline
            .register_plugin(owner.clone(), application_provider_policy())
            .expect("the built-in provider registers once");
        let mut view_model = LauncherViewModel::new();
        view_model.activate();
        // The launcher only reaches `begin_generation` from the query effect,
        // so by then the view model already holds the text. Publishing under an
        // empty query presents nothing at all — that is the empty-query rule,
        // not the intake path this test is about.
        assert_eq!(
            view_model.apply(crikey_ui::UiCommand::SetQuery("fire".to_owned())),
            Some(UiEffect::Query("fire".to_owned()))
        );
        view_model.begin_generation(search_generation);

        let frame = drive_application_provider(&mut pipeline, &owner, "fire", result_items, 17)
            .expect("the admitted current batch produces a frame");
        assert_eq!(frame.generation, search_generation);
        if frame.generation == search_generation {
            view_model.publish(search_generation, search_rows, frame.pending_plugins);
        }
        let published = view_model
            .frame()
            .expect("the successful current frame unlocks UI publication");

        let pipeline_ids = frame
            .rows
            .iter()
            .map(|row| result_identity(row).1.clone())
            .collect::<Vec<_>>();
        let published_ids = published
            .rows
            .iter()
            .map(|row| result_identity(row).1.clone())
            .collect::<Vec<_>>();
        let published_highlights = published
            .rows
            .iter()
            .map(|row| row.highlights.clone())
            .collect::<Vec<_>>();
        assert_eq!(pipeline_ids, expected_ids);
        assert_eq!(published_ids, expected_ids);
        assert_eq!(published_highlights, expected_highlights);

        let diagnostics = pipeline.diagnostics();
        assert_eq!(diagnostics.dispatched_requests, 1);
        assert_eq!(diagnostics.in_flight_requests, 0);
        assert_eq!(diagnostics.rejected_stale_results, 0);
        assert_eq!(pipeline.intake_diagnostics().admitted(), 1);
        assert_eq!(pipeline.intake_diagnostics().merged(), 1);
        assert_eq!(pipeline.intake_depth().batches, 0);
        assert_eq!(
            pipeline.next_wakeup(),
            None,
            "local search must not wait on debounce"
        );
        assert!(pipeline.trace().iter().any(|event| {
            matches!(
                event,
                QueryTraceEvent::Keystroke {
                    at: 17,
                    generation: observed,
                    ..
                } if *observed == search_generation
            )
        }));
        assert!(pipeline.trace().iter().any(|event| {
            matches!(
                event,
                QueryTraceEvent::Dispatched {
                    at: 17,
                    plugin,
                    generation: observed,
                } if plugin == &owner && *observed == search_generation
            )
        }));
        assert!(pipeline.trace().iter().any(|event| {
            matches!(
                event,
                QueryTraceEvent::ResultBatch {
                    at: 17,
                    plugin,
                    generation: observed,
                    items: 2,
                    completion: BatchCompletion::Final,
                } if plugin == &owner && *observed == search_generation
            )
        }));
        assert!(pipeline.trace().iter().any(|event| {
            matches!(
                event,
                QueryTraceEvent::FinalResult {
                    at: 17,
                    plugin,
                    generation: observed,
                    ..
                } if plugin == &owner && *observed == search_generation
            )
        }));
        assert!(pipeline.trace().iter().any(|event| {
            matches!(
                event,
                QueryTraceEvent::Presentation {
                    at: 17,
                    generation: observed,
                    visible_items: 2,
                } if *observed == search_generation
            )
        }));
        assert!(
            !pipeline
                .trace()
                .iter()
                .any(|event| matches!(event, QueryTraceEvent::StaleResultRejected { .. })),
            "the UI boundary must never receive a stale built-in batch"
        );
    }

    /// A common letter matches more applications than one batch may carry, and
    /// the launcher must still say it has finished.
    ///
    /// The built-in provider hands `SearchService`'s hits to the pipeline as a
    /// single `ResultBatch`. Selection admits up to
    /// `max_items_per_plugin_per_query` (250 by default) but the aggregator
    /// refuses any batch over `max_items_per_batch` (50), so on a machine with a
    /// real Start Menu, typing one common letter produced a batch that was
    /// rejected outright: no rows, and — because the refused batch leaves the
    /// request unsettled — `pending_plugins` stayed true, so the launcher sat
    /// there saying "Providers are still responding" with no plugins installed
    /// at all. Rarer letters matched fewer than fifty and worked, which is what
    /// made it look like a slow search rather than a broken one.
    #[test]
    fn a_query_matching_more_applications_than_one_batch_holds_still_settles() {
        use std::collections::BTreeMap;

        use crikey_core::{ArgumentPolicy, Category, HitPolicy, ItemId};

        let owner = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
        let application = |id: &str, label: &str| Item {
            stable_id: ItemId(id.to_owned()),
            plugin_id: owner.clone(),
            category: Category::Application,
            label: label.to_owned(),
            description: format!("launch {label}"),
            target: format!("app://{id}"),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        };

        // Comfortably over the fifty-item batch ceiling, which a Start Menu
        // clears on any common letter.
        let catalog: Vec<Item> = (0..120)
            .map(|index| application(&format!("app-{index}"), &format!("Alpha Application {index}")))
            .collect();

        let mut search = SearchService::new(App::new());
        search
            .complete_stage(StartupStage::WindowAndHotkey)
            .expect("window startup completes");
        search
            .replace_catalog(&owner, 1, catalog)
            .expect("the application catalog is valid");
        search
            .complete_stage(StartupStage::PersistedCatalog)
            .expect("catalog startup completes");
        search
            .complete_stage(StartupStage::AcceptQueries)
            .expect("query startup completes");

        search.submit_query("a").expect("the local query is immediate");
        let matched = search.results().len();
        assert!(
            matched > 50,
            "the fixture must exceed one batch to exercise the ceiling, matched {matched}"
        );
        let result_items = search
            .results()
            .iter()
            .map(|hit| hit.item.clone())
            .collect::<Vec<_>>();

        let mut pipeline = QueryPipeline::new(PipelineConfig::default());
        pipeline
            .register_plugin(owner.clone(), application_provider_policy())
            .expect("the built-in provider registers once");

        let frame = drive_application_provider(&mut pipeline, &owner, "a", result_items, 17)
            .expect("the built-in provider presents a frame");
        assert!(
            !frame.pending_plugins,
            "the built-in catalog is synchronous and has answered, so nothing is \
             outstanding: a frame that still claims to be waiting leaves the \
             launcher spinning until the next keystroke"
        );
        assert!(
            !frame.rows.is_empty(),
            "a query matching {matched} applications must show some of them"
        );
    }

    #[test]
    fn cli_catalog_composition_preserves_foreign_owned_matches() {
        use crikey_core::{ArgumentPolicy, Category, HitPolicy, ItemId};
        use std::collections::BTreeMap;

        let owner = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
        let native = PluginId("native.preview-demo".to_owned());
        let modern = PluginId("modern.catalog-demo".to_owned());
        let item = |plugin: &PluginId, id: &str| Item {
            stable_id: ItemId(id.to_owned()),
            plugin_id: plugin.clone(),
            category: Category::Application,
            label: format!("Preview {id}"),
            description: String::new(),
            target: format!("plugin://{id}"),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        };
        // Exercise both an empty builtin answer and a mixed-owner answer.
        for include_builtin in [false, true] {
            let mut search = SearchService::new(App::new());
            search.complete_stage(StartupStage::WindowAndHotkey).unwrap();
            search
                .replace_catalog(&native, 1, vec![item(&native, "native")])
                .unwrap();
            search
                .replace_catalog(&modern, 1, vec![item(&modern, "modern")])
                .unwrap();
            if include_builtin {
                search
                    .replace_catalog(&owner, 1, vec![item(&owner, "builtin")])
                    .unwrap();
            }
            search.complete_stage(StartupStage::PersistedCatalog).unwrap();
            search.complete_stage(StartupStage::AcceptQueries).unwrap();
            let generation = search.submit_query("preview").unwrap();
            let expected = search.result_rows();
            assert_eq!(expected.len(), if include_builtin { 3 } else { 2 });
            let mut pipeline = QueryPipeline::new(PipelineConfig::default());
            pipeline
                .register_plugin(owner.clone(), application_provider_policy())
                .unwrap();
            let mut retained = RetainedRows::default();
            // The same synchronous composition edge used by UiEffect::Query.
            prepare_catalog_rows(
                &search,
                &mut pipeline,
                &owner,
                "preview",
                generation,
                17,
                &mut retained,
            )
            .expect("foreign catalog owners must not poison builtin intake");
            let mut view_model = LauncherViewModel::new();
            view_model.activate();
            view_model.apply(crikey_ui::UiCommand::SetQuery("preview".to_owned()));
            view_model.begin_generation(generation);
            publish_projected_rows(&search, &retained, &mut view_model, generation);
            let frame = view_model.frame().unwrap();
            assert_eq!(
                frame
                    .rows
                    .iter()
                    .map(|row| (&row.target, &row.highlights))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|row| (&row.target, &row.highlights))
                    .collect::<Vec<_>>(),
            );
            assert!(frame.rows.iter().any(|row| result_identity(row).0 == &native));
            assert!(frame.rows.iter().any(|row| result_identity(row).0 == &modern));
            assert!(!retained.pending());
            assert_eq!(pipeline.diagnostics().in_flight_requests, 0);
            assert_eq!(pipeline.intake_depth().batches, 0);
            assert!(pipeline.take_errors().is_empty());
            assert!(search.results().iter().any(|hit| hit.item.plugin_id == native));
        }
    }

    #[test]
    fn rejected_synchronous_intake_reports_error_and_retires_request() {
        use crikey_core::{ArgumentPolicy, Category, HitPolicy, ItemId};
        let owner = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
        let foreign = Item {
            stable_id: ItemId("foreign".to_owned()),
            plugin_id: PluginId("native.foreign".to_owned()),
            category: Category::Application,
            label: "Foreign".to_owned(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: Default::default(),
            actions: Vec::new(),
        };
        let mut pipeline = QueryPipeline::new(PipelineConfig::default());
        pipeline
            .register_plugin(owner.clone(), application_provider_policy())
            .unwrap();
        let error = drive_application_provider(&mut pipeline, &owner, "foreign", vec![foreign], 17)
            .expect_err("a rejected batch is not a successful empty answer");
        assert!(error.contains("OwnerMismatch"), "{error}");
        assert_eq!(pipeline.diagnostics().in_flight_requests, 0);
        assert_eq!(pipeline.intake_depth().batches, 0);
        let next = drive_application_provider(&mut pipeline, &owner, "next", Vec::new(), 18)
            .expect("a rejected generation must not strand the next query");
        assert!(!next.pending_plugins);
    }

    fn row(plugin: &str, label: &str) -> crikey_ui::ResultRow {
        crikey_ui::ResultRow {
            target: crikey_ui::ActionTarget::Result {
                owner: PluginId(plugin.to_owned()),
                item: ItemId(label.to_owned()),
            },
            source: Some(crikey_ui::ResultSource {
                owner: PluginId(plugin.to_owned()),
                item: ItemId(label.to_owned()),
                kind: crikey_ui::ResultSourceKind::Ordinary,
            }),
            command_state: None,
            label: label.to_owned(),
            description: String::new(),
            icon_reference: None,
            icon: None,
            category: String::new(),
            plugin_name: plugin.to_owned(),
            highlights: Vec::new(),
            argument_hint: None,
            status: None,
            default_action: None,
            alternate_actions: Vec::new(),
        }
    }

    fn result_identity(row: &crikey_ui::ResultRow) -> (&PluginId, &ItemId) {
        match &row.target {
            crikey_ui::ActionTarget::Result { owner, item } => (owner, item),
            crikey_ui::ActionTarget::Command(_) => panic!("expected ordinary result identity"),
        }
    }

    #[test]
    fn native_driver_outcomes_merge_catalog_once_and_prefer_live_suggestions() {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::thread;

        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        fn wait_for<T>(stage: &str, mut observe: impl FnMut() -> Option<T>) -> T {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Some(value) = observe() {
                    return value;
                }
                assert!(
                    Instant::now() < deadline,
                    "native CLI composition timed out: {stage}"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
        // Reuse the real SDK worker, including its empty catalog-only suggestion
        // stream. Build separately, as the existing native preview consumers do.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join("target/cli-catalog-conformance");
        let output = std::process::Command::new("cargo")
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "crikey-plugin-sdk",
                "--example",
                "preview",
                "--target-dir",
            ])
            .arg(&target)
            .output()
            .expect("build real SDK preview worker");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let executable = target
            .join("debug/examples")
            .join(if cfg!(windows) { "preview.exe" } else { "preview" })
            .canonicalize()
            .unwrap();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch(std::env::temp_dir().join(format!(
            "crikey-cli-catalog-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )));
        let package = scratch.0.join("preview-demo");
        std::fs::create_dir_all(&package).unwrap();
        let executable = executable
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        std::fs::write(package.join("crikey.toml"), format!(
            "manifest-version = 1\n[plugin]\nid = \"preview-demo\"\nname = \"Friendly display label\"\nversion = \"0.1.0\"\nruntime = \"native\"\nentrypoint = \"{executable}\"\n",
        )).unwrap();
        let plugin = PluginId("native.preview-demo".to_owned());
        let mut provider_pipeline = QueryPipeline::new(PipelineConfig::default());
        let mut provider = NativeProvider::load_with_collection_window(
            &mut provider_pipeline,
            &[scratch.0.clone()],
            Duration::from_secs(10),
            &DisabledPlugins::default(),
        );
        assert_eq!(
            provider.plugins(),
            &[plugin.clone()],
            "{:?}",
            provider.unavailable()
        );
        provider
            .request_catalog_build(&plugin, 1, Generation::ZERO)
            .unwrap();
        let build = wait_for("catalog", || provider.take_catalog_results().into_iter().next());
        let crikey_app::CatalogBuildResult::Complete(build) = build else {
            panic!("real catalog must complete");
        };
        let mut catalog = build.items;
        // Distinguish stale catalog payloads from live suggestions with identical
        // identities, while making the worker's empty-query mode host-searchable.
        for item in &mut catalog {
            item.description = "Catalog payload".to_owned();
            item.search_terms.push("catalog-only".to_owned());
        }
        let mut catalog_only = catalog[0].clone();
        catalog_only.stable_id = crikey_core::ItemId("catalog-retained".to_owned());
        catalog_only.label = "Preview catalog retained".to_owned();
        catalog.push(catalog_only);
        let mut search = SearchService::new(App::new());
        search.complete_stage(StartupStage::WindowAndHotkey).unwrap();
        search.replace_catalog(&plugin, 1, catalog).unwrap();
        search.complete_stage(StartupStage::PersistedCatalog).unwrap();
        search.complete_stage(StartupStage::AcceptQueries).unwrap();
        let owner = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
        let mut pipeline = QueryPipeline::new(PipelineConfig::default());
        pipeline
            .register_plugin(owner.clone(), application_provider_policy())
            .unwrap();
        let (wake, awakened) = std::sync::mpsc::channel();
        let driver = NativeDriver::spawn(
            provider,
            provider_pipeline,
            Box::new(move |_| {
                let _ = wake.send(());
            }),
        );
        let mut retained = RetainedRows::default();
        for (now, query) in [(17, "catalog-only"), (18, "preview")] {
            let generation = search.submit_query(query).unwrap();
            prepare_catalog_rows(
                &search,
                &mut pipeline,
                &owner,
                query,
                generation,
                now,
                &mut retained,
            )
            .unwrap();
            assert_eq!(
                retained.project_rows(&search, None).0.len(),
                8,
                "catalog rows publish before worker completion"
            );
            retained.mark_pending(generation, RowSource::Native, true);
            driver.submit(generation, query, now, Vec::new(), false, 0);
            let outcome = wait_for("suggestions", || {
                let outcome = driver.take_outcome()?;
                (outcome.generation == generation && !outcome.pending_plugins).then_some(outcome)
            });
            assert!(awakened.recv_timeout(Duration::from_secs(2)).is_ok());
            let expected_suggestions = outcome.rows.clone();
            retained.absorb(
                generation,
                RowSource::Native,
                &outcome.rows,
                outcome.pending_plugins,
            );
            let merged = retained.project_rows(&search, None).0;
            assert_eq!(
                merged.len(),
                8,
                "catalog and suggestions must never duplicate an identity"
            );
            assert!(!retained.pending());
            if query == "catalog-only" {
                assert!(expected_suggestions.is_empty());
                assert!(merged.iter().all(|row| row.description == "Catalog payload"));
            } else {
                assert_eq!(expected_suggestions.len(), 7);
                assert_eq!(result_identity(&merged[0]).1 .0, "catalog-retained");
                assert_eq!(
                    merged[1..]
                        .iter()
                        .map(|row| (&row.target, &row.description))
                        .collect::<Vec<_>>(),
                    expected_suggestions
                        .iter()
                        .map(|row| (&row.target, &row.description))
                        .collect::<Vec<_>>(),
                    "actual suggestion payloads and publication order win over catalog copies",
                );
                assert!(merged[1..].iter().all(|row| row.description != "Catalog payload"));
            }
            assert!(merged.iter().all(|row| result_identity(row).0 == &plugin));
            retained.absorb(Generation::ZERO, RowSource::Native, &[], false);
            assert_eq!(
                retained.project_rows(&search, None).0.len(),
                8,
                "stale completion cannot restore old rows"
            );
        }
    }

    #[test]
    fn retained_rows_merge_ranked_legacy_and_modern_by_source() {
        let search = SearchService::new(App::new());
        let generation = Generation::from_raw(7);
        let mut retained = RetainedRows::default();

        // The synchronous publish, with every async source still out.
        retained.set_builtin(generation, vec![row("builtin.app", "app")]);
        retained.mark_pending(generation, RowSource::Builtin, false);
        retained.mark_pending(generation, RowSource::Files, true);
        retained.mark_pending(generation, RowSource::Legacy, true);
        retained.mark_pending(generation, RowSource::Modern, true);
        assert!(retained.pending(), "outstanding providers keep the frame pending");

        // The legacy supervisor answers with only its own suggestions.
        retained.absorb(
            generation,
            RowSource::Legacy,
            &[row("legacy.files", "file")],
            false,
        );
        // Then the file provider, whose items were ranked into the answer, so
        // its row arrives as part of the ranked group and not beside it.
        retained.set_builtin(
            generation,
            vec![row(FILE_SEARCH_PLUGIN, "report.txt"), row("builtin.app", "app")],
        );
        retained.mark_pending(generation, RowSource::Files, false);
        // The modern supervisor also returns only its own suggestions. Its
        // outcome must not replace the host's newer ranked file answer.
        retained.absorb(generation, RowSource::Modern, &[row("modern.web", "web")], false);

        let sources: Vec<String> = retained
            .project_rows(&search, None)
            .0
            .iter()
            .map(|r| r.plugin_name.clone())
            .collect();
        // Kills two mutations at once: "each supervisor's publish clobbers the
        // other" would leave only the ranked group + `modern.web`, and
        // "rebuild the ranked group from the provider's frame" would lose the
        // file row.
        assert_eq!(
            sources,
            vec![FILE_SEARCH_PLUGIN, "builtin.app", "legacy.files", "modern.web"],
            "the ranked group keeps its own order, and legacy and modern rows follow it",
        );
        assert!(
            !retained.pending(),
            "once every source has answered the merged frame is no longer pending",
        );
    }

    /// A file backend that takes the query and does not answer, the way a hung
    /// mount or an index socket that never replies does. `FILE_SEARCH_DEADLINE`
    /// is a promise the backend is asked to keep, not one the launcher can
    /// enforce.
    ///
    /// It returns on cancellation only so that dropping the driver at the end of
    /// the test can join the worker thread; the call is still outstanding for
    /// the whole of the test proper.
    struct WedgedFileSearch {
        entered: std::sync::mpsc::Sender<()>,
    }

    impl crikey_app::FileItemSearch for WedgedFileSearch {
        fn search_file_items(
            &self,
            _plugin: &PluginId,
            query: &crikey_platform::FileSearchQuery,
        ) -> Option<crikey_core::Result<(Vec<Item>, crikey_platform::FileSearchResults)>> {
            let _ = self.entered.send(());
            while !query.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            None
        }
    }

    /// The readiness invariant, end to end: a source that marks itself pending
    /// settles that generation.
    ///
    /// The file source is the only one whose pending mark nothing else can
    /// clear — no later query for the same generation, no sibling provider — so
    /// a generation it takes on and never answers leaves the launcher saying
    /// "Providers are still responding" for the rest of the session.
    #[test]
    fn a_file_source_marked_pending_settles_even_when_its_backend_never_answers() {
        let generation = Generation::from_raw(9);
        let owner = PluginId(FILE_SEARCH_PLUGIN.to_owned());
        let (entered, entries) = std::sync::mpsc::channel();
        let search = WedgedFileSearch { entered };
        let mut pipeline = crikey_app::QueryPipeline::new(crikey_app::PipelineConfig::default());
        pipeline
            .register_plugin(owner.clone(), file_provider::file_provider_policy())
            .expect("the file provider registers once");
        let driver = file_provider::FileSearchDriver::spawn(
            owner.clone(),
            pipeline,
            Box::new(move || Box::new(search)),
            || {},
        );

        // Exactly the query arm's order: hand the query over, then mark the
        // source pending on the driver's own answer to whether it took it.
        let mut search_service = ranked_service(&[]);
        let mut retained = RetainedRows::default();
        let files_outstanding = driver.submit(generation, "quarterly", 1_000);
        retained.set_builtin(generation, vec![row("builtin.app", "app")]);
        retained.mark_pending(generation, RowSource::Files, files_outstanding);
        assert!(
            files_outstanding && retained.pending(),
            "the frame is pending while the file source owes an answer, or this test proves nothing"
        );
        // Provably inside the backend: the worker is stuck, not merely slow to
        // start.
        entries.recv().expect("the worker reaches the backend");

        // A later turn of the event loop, a minute on — long past any deadline
        // the driver could still be waiting on.
        if let Some(outcome) = driver.take_outcome(61_000) {
            absorb_file_items(&mut search_service, &mut retained, &owner, outcome);
        }

        assert!(
            !retained.pending(),
            "a generation the file source took on must settle, even with no answer from the backend"
        );
        let sources: Vec<String> = retained
            .project_rows(&search_service, None)
            .0
            .iter()
            .map(|r| r.plugin_name.clone())
            .collect();
        assert_eq!(
            sources,
            ["builtin.app"],
            "settling contributes no file rows and removes none of the ranked group"
        );
    }

    #[test]
    fn pinned_calculator_identity_wins_without_hiding_foreign_same_id() {
        let search = SearchService::new(App::new());
        let generation = Generation::from_raw(3);
        let mut calculator = Calculator::new(None);
        let calculated = calculator.rows(generation, "2+2");
        let mut echoed = calculated[0].clone();
        echoed.description = "Echoed calculator payload".to_owned();
        let mut foreign = echoed.clone();
        foreign.target = crikey_ui::ActionTarget::Result {
            owner: PluginId("native.other".to_owned()),
            item: result_identity(&foreign).1.clone(),
        };
        foreign.source.as_mut().unwrap().owner = PluginId("native.other".to_owned());
        foreign.label = "Foreign item with calculator's local ID".to_owned();
        let mut retained = RetainedRows::default();
        retained.set_calculated(generation, calculated.clone());
        retained.set_builtin(generation, vec![echoed.clone(), foreign.clone()]);
        retained.absorb(generation, RowSource::Native, &[echoed], false);
        let merged = retained.project_rows(&search, None).0;
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].target, calculated[0].target);
        assert_eq!(merged[0].description, calculated[0].description);
        assert_eq!(merged[1].target, foreign.target);
        assert_eq!(merged[1].label, foreign.label);
    }

    #[test]
    fn a_calculated_row_leads_the_frame_and_survives_an_asynchronous_fold() {
        let search = SearchService::new(App::new());
        let generation = Generation::from_raw(3);
        let mut calculator = Calculator::new(None);
        // Exactly what the query arm builds: the calculator's answer in its own
        // group, then the catalog's ranked rows.
        let mut retained = RetainedRows::default();
        retained.set_calculated(generation, calculator.rows(generation, "2+2"));
        retained.set_builtin(generation, vec![row("builtin.app", "app")]);
        retained.mark_pending(generation, RowSource::Legacy, true);

        // The legacy supervisor republishes the rows it was handed ahead of its
        // own. A fold that rebuilt the host's groups from that frame would have
        // to classify the calculated row correctly to keep it; keeping those
        // groups out of the fold entirely is what makes that impossible to get
        // wrong.
        let mut published = retained.project_rows(&search, None).0;
        published.push(row("legacy.files", "file"));
        retained.absorb(generation, RowSource::Legacy, &published, false);

        let merged = retained.project_rows(&search, None).0;
        let labels: Vec<&str> = merged.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["4", "app", "file"],
            "the calculated row leads the frame and outlives the legacy fold"
        );
        assert_eq!(merged[0].plugin_name, calculator::CALCULATOR_PLUGIN);
        assert!(
            !retained.pending(),
            "the calculator answers synchronously and never owes the frame anything"
        );
    }

    #[test]
    fn retained_rows_drop_a_superseded_generation() {
        let search = SearchService::new(App::new());
        let old = Generation::from_raw(1);
        let new = Generation::from_raw(2);
        let mut retained = RetainedRows::default();

        retained.absorb(old, RowSource::Legacy, &[row("legacy.old", "x")], false);
        // A newer generation's first fold drops every stale group first, so an
        // older generation's rows never cross the boundary. Kills the "reuse
        // retained rows across generations" mutation.
        retained.absorb(new, RowSource::Modern, &[row("modern.new", "y")], false);

        let sources: Vec<String> = retained
            .project_rows(&search, None)
            .0
            .iter()
            .map(|r| r.plugin_name.clone())
            .collect();
        assert_eq!(
            sources,
            vec!["modern.new"],
            "stale legacy rows from an older generation are dropped",
        );
    }

    /// A discovered application, as the host's own catalog publishes it.
    fn application(label: &str) -> Item {
        let owner = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
        let category = crikey_core::Category::Application;
        Item {
            stable_id: crikey_core::ItemId::derived(&owner, &category, label),
            plugin_id: owner,
            category,
            label: label.to_owned(),
            description: String::new(),
            target: format!("/usr/bin/{label}"),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            hit_policy: crikey_core::HitPolicy::Recorded,
            score_hint: 0,
            metadata: Default::default(),
            actions: Vec::new(),
        }
    }

    /// A search service that has reached the query-accepting stage with
    /// `labels` in its application catalog.
    fn ranked_service(labels: &[&str]) -> SearchService {
        let mut service = SearchService::new(App::new());
        for (stage, next) in [
            (StartupStage::WindowAndHotkey, StartupStage::PersistedCatalog),
            (StartupStage::PersistedCatalog, StartupStage::AcceptQueries),
            (StartupStage::AcceptQueries, StartupStage::RequiredWorkers),
        ] {
            assert_eq!(service.complete_stage(stage), Ok(Some(next)));
        }
        service
            .replace_catalog(
                &PluginId(APPLICATION_CATALOG_PLUGIN.to_owned()),
                1,
                labels.iter().map(|label| application(label)).collect(),
            )
            .expect("the application catalog accepts its items");
        service
    }

    #[test]
    fn rapid_builtin_queries_cap_ranked_matches_before_one_item_intake_boundary() {
        let mut search = ranked_service(&["amber", "amethyst", "amaranth", "alpine"]);
        let owner = PluginId(APPLICATION_CATALOG_PLUGIN.into());
        let mut config = PipelineConfig::default();
        config.limits.max_items_per_query = 1;
        config.intake_limits.capacity_items = 1;
        config.intake_limits.capacity_batches = 1;
        let mut pipeline = QueryPipeline::new(config);
        pipeline
            .register_plugin(owner.clone(), application_provider_policy())
            .unwrap();
        let mut retained = RetainedRows {
            max_results: Some(1),
            ..RetainedRows::default()
        };
        let mut model = LauncherViewModel::new();
        model.activate();
        for (index, query) in ["a", "am", "amb", "a", "", "am", "no-match-at-all", "a"]
            .into_iter()
            .enumerate()
        {
            model.apply(crikey_ui::UiCommand::SetQuery(query.into()));
            // Adjacent keystrokes deliberately share a scheduler timestamp.
            let (generation, catalog) = admit_root_query(
                &mut search,
                &mut pipeline,
                &owner,
                &mut model,
                &mut retained,
                query,
                index as u64 / 2,
            )
            .unwrap();
            catalog.expect("bounded ranked builtin publication must settle without boundary refusal");
            if matches!(query, "a" | "am") {
                assert!(
                    search.results().len() > 1,
                    "exercise multiple genuine catalog matches"
                );
            }
            let expected = search.result_rows().first().map(|row| row.target.clone());
            publish_root_rows(&search, None, &mut retained, &mut model, generation, query, None);
            let frame = model.frame().unwrap();
            assert_eq!(
                frame.rows.first().map(|row| row.target.clone()),
                expected,
                "retain the first ranked match rather than discarding an oversized answer"
            );
            assert!(frame.rows.len() <= 1);
            assert!(!frame.pending_plugins);
            assert_eq!(pipeline.diagnostics().in_flight_requests, 0);
            assert_eq!(pipeline.intake_depth().batches, 0);
            assert!(pipeline.take_errors().is_empty());
        }
    }

    /// The file items a backend would produce for `names`, through the real
    /// mapping, so their ids, categories and actions are the ones the launcher
    /// actually has to rank.
    fn file_items(names: &[&str]) -> Vec<Item> {
        let hits: Vec<crikey_platform::FileHit> = names
            .iter()
            .map(|name| crikey_platform::FileHit {
                name: (*name).to_owned(),
                path: crikey_core::PlatformPath::from(std::path::PathBuf::from(format!("/home/user/{name}"))),
                kind: crikey_platform::FileKind::File,
                modified_unix_seconds: None,
            })
            .collect();
        crikey_platform::file_items(&PluginId(FILE_SEARCH_PLUGIN.to_owned()), &hits)
    }

    /// The whole point of ranking the two together.
    ///
    /// `mor` is a prefix of "Mortgage Analysis.pdf" and merely a substring of
    /// "Memory Diagnostics Tool". The match-method bands put a prefix above a
    /// substring, and the file category's lower weight is not enough to close
    /// that gap — but only if both are scored in the same pass. While file rows
    /// were concatenated after the catalog's, the application came first no
    /// matter what the user typed or how often they chose the file.
    #[test]
    fn a_better_matching_file_outranks_an_application() {
        let owner = PluginId(FILE_SEARCH_PLUGIN.to_owned());
        let mut search = ranked_service(&["Memory Diagnostics Tool"]);
        let generation = search.submit_query("mor").expect("the query is accepted");
        let mut retained = RetainedRows::default();
        retained.set_builtin(generation, search.result_rows());
        retained.mark_pending(generation, RowSource::Files, true);
        assert_eq!(
            retained
                .project_rows(&search, None)
                .0
                .iter()
                .map(|row| row.label.clone())
                .collect::<Vec<String>>(),
            ["Memory Diagnostics Tool"],
            "the catalog answers first, or this test proves nothing"
        );

        absorb_file_items(
            &mut search,
            &mut retained,
            &owner,
            file_provider::FileOutcome {
                generation,
                items: file_items(&["Mortgage Analysis.pdf"]),
                pending: false,
                state: crikey_app::OrdinaryProviderState::Matches,
            },
        );

        let labels: Vec<String> = retained
            .project_rows(&search, None)
            .0
            .iter()
            .map(|row| row.label.clone())
            .collect();
        assert_eq!(
            labels,
            ["Mortgage Analysis.pdf", "Memory Diagnostics Tool"],
            "a prefix-matching file outranks a substring-matching application"
        );
        assert!(!retained.pending(), "the merged answer settles the file source");
    }

    /// The merged file item is in the ranked answer, so `execute` finds it.
    ///
    /// This is what let the launcher drop its second execution path: the
    /// driver's own generation-scoped item store and the `execute_host_action`
    /// entry point it fed. Both existed only because a file item was not in
    /// `results`, and both would have been a second copy of the host-mediated
    /// gate to keep in step.
    ///
    /// Asserted through a *nonexistent* action rather than the real open, so
    /// the assertion is about the lookup and nothing on this machine is
    /// launched. The two refusals are the two answers to "is this item in the
    /// ranked answer at all".
    #[test]
    fn a_merged_file_item_is_found_by_the_execution_path() {
        let owner = PluginId(FILE_SEARCH_PLUGIN.to_owned());
        let mut search = ranked_service(&["Memory Diagnostics Tool"]);
        let generation = search.submit_query("mor").expect("the query is accepted");
        let items = file_items(&["Mortgage Analysis.pdf"]);
        let file_id = items[0].stable_id.clone();
        let absent = crikey_core::ItemId("no such item".to_owned());
        let action = crikey_core::ActionId("no such action".to_owned());

        let mut retained = RetainedRows::default();
        retained.set_builtin(generation, search.result_rows());
        absorb_file_items(
            &mut search,
            &mut retained,
            &owner,
            file_provider::FileOutcome {
                generation,
                items,
                pending: false,
                state: crikey_app::OrdinaryProviderState::Matches,
            },
        );

        let found = search
            .execute(&file_id, &action)
            .expect_err("the action does not exist");
        assert!(
            found.to_string().contains("action"),
            "a merged file item must be found in the ranked answer and refused on its action, got {found}"
        );
        let missing = search
            .execute(&absent, &action)
            .expect_err("the item does not exist");
        assert!(
            missing.to_string().contains("result"),
            "an item that is not in the answer is refused on the item, got {missing}"
        );
    }

    /// A file item that does not answer the current query is dropped, not
    /// ranked to the bottom.
    #[test]
    fn a_file_that_does_not_match_the_query_is_not_shown() {
        let owner = PluginId(FILE_SEARCH_PLUGIN.to_owned());
        let mut search = ranked_service(&["Memory Diagnostics Tool"]);
        let generation = search.submit_query("mor").expect("the query is accepted");
        let mut retained = RetainedRows::default();
        retained.set_builtin(generation, search.result_rows());

        absorb_file_items(
            &mut search,
            &mut retained,
            &owner,
            file_provider::FileOutcome {
                generation,
                items: file_items(&["Mortgage Analysis.pdf", "budget.ods"]),
                pending: false,
                state: crikey_app::OrdinaryProviderState::Matches,
            },
        );

        let labels: Vec<String> = retained
            .project_rows(&search, None)
            .0
            .iter()
            .map(|row| row.label.clone())
            .collect();
        assert_eq!(
            labels,
            ["Mortgage Analysis.pdf", "Memory Diagnostics Tool"],
            "a file the query does not match contributes no row at all"
        );
    }

    /// A late file batch must not move what the user has selected.
    ///
    /// Driven through the view model's own publish path, because that is where
    /// the anchoring lives: the selection follows its `ItemId` wherever the
    /// republished rows put it. Ranking file items in reorders the list under
    /// the user's cursor far more readily than appending them ever did, so this
    /// is the property that had to survive the change.
    #[test]
    fn a_late_file_batch_does_not_move_the_selection() {
        let owner = PluginId(FILE_SEARCH_PLUGIN.to_owned());
        let mut search = ranked_service(&["Memory Diagnostics Tool", "Morse Trainer"]);
        let mut view_model = LauncherViewModel::new();
        view_model.activate();
        let Some(UiEffect::Query(raw)) = view_model.apply(crikey_ui::UiCommand::SetQuery("mor".to_owned()))
        else {
            panic!("typing a query asks the host to run it");
        };

        let generation = search.submit_query(&raw).expect("the query is accepted");
        view_model.begin_generation(generation);
        let mut retained = RetainedRows::default();
        retained.set_builtin(generation, search.result_rows());
        retained.mark_pending(generation, RowSource::Files, true);
        publish_projected_rows(&search, &retained, &mut view_model, generation);

        // The user moves off the first row while the file search is still out.
        // Row 1 is the substring match, which the merge will push down a place.
        view_model.apply(crikey_ui::UiCommand::SelectNext);
        let selected = view_model
            .frame()
            .expect("the selection is a change worth drawing");
        assert_eq!(selected.selected, 1);
        let chosen = selected.rows[1].target.clone();

        absorb_file_items(
            &mut search,
            &mut retained,
            &owner,
            file_provider::FileOutcome {
                generation,
                items: file_items(&["Mortgage Analysis.pdf"]),
                pending: false,
                state: crikey_app::OrdinaryProviderState::Matches,
            },
        );
        publish_projected_rows(&search, &retained, &mut view_model, generation);

        let frame = view_model.frame().expect("the merged answer is a new frame");
        assert_eq!(
            frame
                .rows
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<&str>>(),
            [
                "Morse Trainer",
                "Mortgage Analysis.pdf",
                "Memory Diagnostics Tool"
            ],
            "the file really did land above the row the user was on, or this test proves nothing"
        );
        assert_eq!(
            frame.selected, 2,
            "the selection followed its item down the list rather than staying at index 1"
        );
        assert_eq!(
            frame.rows[frame.selected].target, chosen,
            "the selection stays on the item the user chose, wherever the merge moved it"
        );
    }

    // -----------------------------------------------------------------------
    // Startup recovery: when this launch counts as ready (spec 24.2)
    // -----------------------------------------------------------------------

    /// A private directory removed when the test that made it ends.
    struct Scratch {
        path: std::path::PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("crikey-cli-{label}-{}-{unique}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the scratch directory is creatable");
            Self { path }
        }

        fn journal(&self) -> std::path::PathBuf {
            self.path.join("startup.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    #[cfg(unix)]
    #[test]
    fn private_cache_rejects_a_symlink_leaf() {
        let scratch = Scratch::new("private-cache-symlink");
        let target = scratch.path.join("target");
        let link = scratch.path.join("link");
        std::fs::create_dir(&target).expect("the cache target is creatable");
        std::os::unix::fs::symlink(&target, &link).expect("the cache symlink is creatable");

        assert!(
            create_private_dir(&link).is_err(),
            "a trust-root helper must not follow a symlink to another directory"
        );
    }

    /// A launch in progress whose journal already carries one failure short of
    /// the safe-mode threshold, with this attempt recorded on disk.
    ///
    /// One more unfinished attempt is exactly what tips the next launch into
    /// safe mode, so the two tests below read a mode rather than a counter:
    /// `SafeMode` means this launch was never credited as ready, `Normal`
    /// means it was.
    fn ledger_mid_startup(path: &std::path::Path) -> Rc<RefCell<StartupLedger>> {
        for _ in 1..crikey_app::SAFE_MODE_AFTER_FAILURES {
            let mut earlier = StartupJournal::load(path);
            earlier.begin_startup(&[]);
            earlier.save().expect("the scratch journal is writable");
        }
        let mut journal = StartupJournal::load(path);
        journal.begin_startup(&[PluginId(APPLICATION_CATALOG_PLUGIN.to_owned())]);
        journal.save().expect("the scratch journal is writable");
        StartupLedger::new(Some(journal))
    }

    /// The mode the *next* launch would be admitted under, read from disk.
    fn next_launch_mode(path: &std::path::Path) -> StartupMode {
        StartupJournal::load(path).begin_startup(&[])
    }

    /// A renderer that dies in `GraphicsState::new` delivers no event, and that
    /// launch must stay counted as a failure so the loop reaches safe mode.
    ///
    /// Kills the mutation that marks ready beside `request_activation`: a
    /// queued activation is not a started renderer, and crediting it persists
    /// zero failures for every launch that dies in renderer startup.
    #[test]
    fn a_launch_whose_renderer_never_delivers_an_event_stays_recorded_as_a_failure() {
        let scratch = Scratch::new("renderer-never-started");
        let path = scratch.journal();
        let ledger = ledger_mid_startup(&path);

        let mut callback = ready_on_first_event(Rc::clone(&ledger), |_| {
            unreachable!("this renderer never reached its event loop")
        });
        let _ = &mut callback;

        assert_eq!(
            next_launch_mode(&path),
            StartupMode::SafeMode {
                consecutive_failures: crikey_app::SAFE_MODE_AFTER_FAILURES
            },
            "a repeated renderer-startup crash must reach safe mode",
        );
    }

    /// The first event the running event loop delivers marks the launch ready,
    /// and is still passed through to the launcher's own handler.
    #[test]
    fn the_first_event_from_the_running_event_loop_marks_the_launch_ready() {
        let scratch = Scratch::new("renderer-started");
        let path = scratch.journal();
        let ledger = ledger_mid_startup(&path);

        let seen = Rc::new(RefCell::new(Vec::new()));
        let recorder = Rc::clone(&seen);
        let mut callback = ready_on_first_event(Rc::clone(&ledger), move |event| {
            recorder.borrow_mut().push(event);
        });
        callback(NativeLauncherEvent::Activated);

        assert_eq!(
            next_launch_mode(&path),
            StartupMode::Normal,
            "a renderer that reached its event loop clears the failure run",
        );
        assert!(
            matches!(seen.borrow().as_slice(), [NativeLauncherEvent::Activated]),
            "the readiness wrapper still delivers the event it observed",
        );
    }

    /// Each provider's plugins are on disk before the next provider loads, so a
    /// crash between two of them names what was active at that moment.
    ///
    /// Kills the mutation that records the plugin set only once, after all
    /// three providers are up: everything that crashed earlier then leaves a
    /// record naming only the built-in catalog.
    #[test]
    fn plugins_are_recorded_as_each_provider_becomes_active_without_charging_a_second_attempt() {
        let scratch = Scratch::new("provider-progress");
        let path = scratch.journal();
        let ledger = ledger_mid_startup(&path);
        let builtin = PluginId(APPLICATION_CATALOG_PLUGIN.to_owned());
        let legacy = PluginId("legacy.alpha".to_owned());

        ledger
            .borrow_mut()
            .record_active(&[builtin.clone(), legacy.clone()]);

        let recorded = StartupJournal::load(&path);
        assert_eq!(
            recorded.active_during_abnormal_shutdown(),
            [builtin, legacy],
            "a crash after the legacy provider must name the legacy plugin too",
        );
        assert_eq!(
            next_launch_mode(&path),
            StartupMode::SafeMode {
                consecutive_failures: crikey_app::SAFE_MODE_AFTER_FAILURES
            },
            "refreshing the plugin set repeats the verdict; it never charges a second attempt",
        );
    }

    /// The failure this clamp exists for. Recency is scored as
    /// `now - last_selected`, saturating at zero, so a clock that steps
    /// backwards makes every recent selection look like it happened in the
    /// future: the subtraction saturates and every item collapses to the same
    /// maximum recency, which is the ranking going flat exactly when the user
    /// notices. Kills the obvious implementation, `set_history_time(now())`.
    #[test]
    fn a_backwards_clock_step_never_lowers_the_history_time() {
        let mut clock = HistoryClock::default();

        assert_eq!(clock.observe(1_700_000_000), 1_700_000_000);
        assert_eq!(
            clock.observe(1_699_000_000),
            1_700_000_000,
            "an NTP correction that moves the clock back must not move ranking back"
        );
        assert_eq!(
            clock.observe(0),
            1_700_000_000,
            "a clock that cannot be read at all reads as the epoch and must be ignored"
        );
        assert_eq!(
            clock.observe(1_700_000_042),
            1_700_000_042,
            "the clamp must resume tracking once the clock passes the highest value seen"
        );
    }

    /// The clock must start from real wall time rather than the zero a fresh
    /// `SearchService` holds: at zero, every restored selection is dated
    /// decades in the future and the recency term contributes nothing at all.
    #[test]
    fn the_history_clock_starts_from_real_wall_clock_seconds() {
        // 2020-01-01, comfortably after any plausible build and before any
        // plausible run, so this cannot rot into a tautology.
        const AFTER_2020: u64 = 1_577_836_800;

        let mut clock = HistoryClock::default();
        assert!(
            clock.advance() > AFTER_2020,
            "the first sample must be a real Unix timestamp"
        );
    }

    #[test]
    fn declared_rows_keep_reference_identity_without_superseding_ordinary_sources() {
        let search = SearchService::new(App::new());
        let generation = Generation::from_raw(1);
        let ordinary = row("native.owner", "same item");
        let mut first = ordinary.clone();
        first.target = crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
            provider_id: PluginId("native.owner".into()),
            command_id: "first".into(),
        });
        let mut second = first.clone();
        second.target = crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
            provider_id: PluginId("native.owner".into()),
            command_id: "second".into(),
        });
        let mut retained = RetainedRows::default();
        retained.set_builtin(generation, vec![ordinary.clone()]);
        retained.set_commands(generation, vec![second.clone(), first.clone(), second.clone()]);
        let merged = retained.project_rows(&search, None).0;
        assert_eq!(
            merged.iter().map(|row| &row.target).collect::<Vec<_>>(),
            vec![&ordinary.target, &second.target, &first.target]
        );
        retained.max_results = Some(2);
        assert_eq!(retained.project_rows(&search, None).0.len(), 2);
        retained.begin(Generation::from_raw(2));
        assert_eq!(retained.max_results, Some(2));
    }

    #[test]
    fn aliases_precede_ordinary_rows_before_one_result_cap_and_deduplication() {
        let search = SearchService::new(App::new());
        let generation = Generation::from_raw(1);
        let ordinary = row("owner", "ordinary");
        let mut alias = row("owner", "canonical");
        alias.target = crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
            provider_id: PluginId("owner".into()),
            command_id: "command".into(),
        });
        alias.command_state = Some(crikey_ui::CommandRowState {
            registry_revision: 1,
            preference_revision: 1,
            available: true,
            unavailable_reason: None,
            favorite: None,
            favorite_count: 0,
            hidden: false,
            fallback: None,
            fallback_count: 0,
            aliases: vec!["go".into()],
            hotkey: None,
            argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            argument: None,
            ranking_reset_available: true,
        });
        let mut retained = RetainedRows::default();
        retained.set_builtin(generation, vec![ordinary]);
        retained.set_commands(generation, vec![alias.clone(), alias.clone()]);
        retained.alias_query = "go".into();
        retained.max_results = Some(1);
        assert_eq!(retained.project_rows(&search, None).0[0].target, alias.target);
        retained.max_results = None;
        assert_eq!(
            retained.project_rows(&search, None).0.len(),
            2,
            "alias is not repeated in the ordinary command group"
        );
    }

    #[test]
    fn fallback_requires_current_terminal_empty_evidence_and_retains_literal_argument() {
        use crikey_app::OrdinaryProviderState;
        let mut model = LauncherViewModel::new();
        model.activate();
        let (view, _) = model.view().unwrap();
        let generation = Generation::from_raw(9);
        let literal = "  'literal'; $(not-a-shell) & trailing  ";
        for state in [
            OrdinaryProviderState::Pending,
            OrdinaryProviderState::Failed,
            OrdinaryProviderState::Cancelled,
            OrdinaryProviderState::Matches,
            OrdinaryProviderState::Empty,
        ] {
            let summary = crikey_app::OrdinarySearchSummary {
                view: view.get(),
                generation,
                providers: vec![crikey_app::OrdinarySearchProvider {
                    plugin: PluginId("native.owner".into()),
                    state,
                }],
            };
            let search = compose_fallback_search(
                literal,
                view,
                generation,
                OrdinaryProviderState::Empty,
                &[Some(summary)],
            )
            .unwrap();
            assert_eq!(
                search.argument(),
                (state == OrdinaryProviderState::Empty).then_some(literal)
            );
        }
        let stale = crikey_app::OrdinarySearchSummary {
            view: view.get(),
            generation: Generation::from_raw(8),
            providers: vec![crikey_app::OrdinarySearchProvider {
                plugin: PluginId("native.owner".into()),
                state: OrdinaryProviderState::Empty,
            }],
        };
        assert!(!compose_fallback_search(
            literal,
            view,
            generation,
            OrdinaryProviderState::Empty,
            &[Some(stale)]
        )
        .unwrap()
        .eligible());
        assert!(
            !compose_fallback_search(literal, view, generation, OrdinaryProviderState::Empty, &[None])
                .unwrap()
                .eligible()
        );
    }

    #[test]
    fn selective_reset_pending_defers_success_publication_without_losing_exact_command() {
        let mut search = SearchService::new(App::new());
        let reference = search.command_snapshot().commands.keys().next().unwrap().clone();
        let writer = command_history::HistoryWorker::spawn(None, || {}).unwrap();
        let mut deferred = Vec::new();
        publish_learning(
            command_history::Learning::Host(reference.clone(), 42),
            &mut search,
            &writer,
            true,
            &mut deferred,
        )
        .unwrap();
        assert!(search.selection_history_snapshot().commands.is_empty());
        assert_eq!(deferred.len(), 1);
        for learning in deferred {
            learning.publish(&mut search).unwrap();
        }
        let snapshot = search.selection_history_snapshot();
        assert_eq!(snapshot.commands.len(), 1);
        assert_eq!(snapshot.commands[0].reference, reference);
        assert_eq!(snapshot.commands[0].frequency, 1);
    }

    #[test]
    fn registry_refresh_first_cli_publication_already_contains_the_selected_command() {
        let mut search = SearchService::new(App::new());
        for stage in [
            StartupStage::WindowAndHotkey,
            StartupStage::PersistedCatalog,
            StartupStage::AcceptQueries,
        ] {
            search.complete_stage(stage).unwrap();
        }
        let owner = PluginId("builtin.refresh-fixture".into());
        search
            .replace_catalog(
                &owner,
                1,
                vec![Item {
                    stable_id: ItemId("ordinary".into()),
                    plugin_id: owner.clone(),
                    category: crikey_core::Category::Command,
                    label: "Settings ordinary".into(),
                    description: String::new(),
                    target: String::new(),
                    search_terms: Vec::new(),
                    icon_reference: None,
                    argument_policy: crikey_core::ArgumentPolicy::Forbidden,
                    hit_policy: crikey_core::HitPolicy::Recorded,
                    score_hint: 0,
                    metadata: std::collections::BTreeMap::new(),
                    actions: Vec::new(),
                }],
            )
            .unwrap();
        let reference = search.command_snapshot().commands.keys().next().unwrap().clone();
        let mut model = LauncherViewModel::new();
        model.activate();
        model.apply(crikey_ui::UiCommand::SetQuery("settings".into()));
        let mut retained = RetainedRows::default();
        let first = search.submit_query("settings").unwrap();
        model.begin_generation(first);
        retained.set_calculated(first, Vec::new());
        retained.set_builtin(first, search.result_rows());
        publish_root_rows(&search, None, &mut retained, &mut model, first, "settings", None);
        model.apply(crikey_ui::UiCommand::SelectNext);
        let selected = model.frame().unwrap();
        assert_eq!(
            selected.rows[selected.selected].target,
            crikey_ui::ActionTarget::Command(reference.clone())
        );

        let query = model.refresh_root_query().unwrap();
        let refreshed = search.submit_query(&query).unwrap();
        model.begin_generation(refreshed);
        retained.set_calculated(refreshed, Vec::new());
        retained.set_builtin(refreshed, search.result_rows());
        assert!(
            retained.commands.is_empty(),
            "a new retained generation retires the old command group"
        );
        // This is the production query handler's FIRST publication, before
        // the later terminal/evidence composition pass.
        publish_root_rows(&search, None, &mut retained, &mut model, refreshed, &query, None);
        let first_refresh = model.frame().unwrap();
        assert_eq!(
            first_refresh.rows[first_refresh.selected].target,
            crikey_ui::ActionTarget::Command(reference.clone())
        );
        publish_root_rows(&search, None, &mut retained, &mut model, refreshed, &query, None);
        assert!(matches!(model.apply(crikey_ui::UiCommand::ExecuteDefault),
            Some(UiEffect::InvokeCommand { reference: invoked, generation, .. })
                if invoked == reference && generation == refreshed));
    }

    #[test]
    fn saved_effective_result_limit_reprojects_aliases_before_live_global_cap() {
        use crikey_platform::{DirectoryConvention, DirectoryEnvironment};
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let scratch = Scratch(std::env::temp_dir().join(format!(
            "crikey-live-command-cap-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        std::fs::create_dir_all(&scratch.0).unwrap();
        std::fs::write(scratch.0.join("config.toml"), "[launcher]\nmax-results = 6\n").unwrap();
        let environment = [
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "CRIKEY_CONFIG_DIR",
            "CRIKEY_DATA_DIR",
            "CRIKEY_CACHE_DIR",
            "CRIKEY_STATE_DIR",
        ]
        .into_iter()
        .fold(DirectoryEnvironment::new(), |environment, key| {
            environment.set(key, scratch.0.as_os_str())
        });
        let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment).unwrap();
        let store = ConfigStore::load_with_policy(&directories, None).unwrap();
        let mut configuration = LauncherConfiguration::from_store(store);
        let mut search = SearchService::new(App::new());
        for stage in [
            StartupStage::WindowAndHotkey,
            StartupStage::PersistedCatalog,
            StartupStage::AcceptQueries,
        ] {
            search.complete_stage(stage).unwrap();
        }
        let reference = search.command_snapshot().commands.keys().next().unwrap().clone();
        let mut preferences = search.command_preferences_snapshot();
        Arc::make_mut(&mut preferences).preferences.records.push(
            crikey_config::command_preferences::CommandPreference {
                reference: reference.clone(),
                hidden: false,
                alias: Some("cap-alias".into()),
                hotkey: None,
            },
        );
        search.publish_command_preferences(preferences);
        let mut model = LauncherViewModel::new();
        model.activate();
        model.apply(crikey_ui::UiCommand::SetQuery("cap-alias".into()));
        let generation = search.submit_query("cap-alias").unwrap();
        model.begin_generation(generation);
        let mut retained = RetainedRows::default();
        retained.configure(&configuration.store);
        retained.set_builtin(
            generation,
            (0..6)
                .map(|index| row("ordinary", &format!("row-{index}")))
                .collect(),
        );
        publish_root_rows(
            &search,
            Some(&configuration.store),
            &mut retained,
            &mut model,
            generation,
            "cap-alias",
            None,
        );
        assert_eq!(model.frame().unwrap().rows.len(), 6);

        settings::persist(&mut configuration.store, KEY_MAX_RESULTS, "1").unwrap();
        configuration.reloaded = true;
        assert!(configuration.take_reloaded());
        assert!(
            retained.configure(&configuration.store),
            "authoritative reload owes a new projection"
        );
        publish_root_rows(
            &search,
            Some(&configuration.store),
            &mut retained,
            &mut model,
            generation,
            "cap-alias",
            None,
        );
        let capped = model.frame().unwrap();
        assert_eq!(capped.rows.len(), 1);
        assert_eq!(
            capped.rows[0].target,
            crikey_ui::ActionTarget::Command(reference.clone())
        );
        assert_eq!(
            ConfigStore::load_with_policy(&directories, None)
                .unwrap()
                .get(KEY_MAX_RESULTS),
            Some("1")
        );

        // Registration/presentation consume the winning layer, never the saved draft.
        configuration.store =
            ConfigStore::load_with_overrides(&directories, None, &[(KEY_MAX_RESULTS.into(), "3".into())])
                .unwrap();
        assert!(retained.configure(&configuration.store));
        publish_root_rows(
            &search,
            Some(&configuration.store),
            &mut retained,
            &mut model,
            generation,
            "cap-alias",
            None,
        );
        let shadowed = model.frame().unwrap();
        assert_eq!(shadowed.rows.len(), 3);
        assert_eq!(
            shadowed.rows[0].target,
            crikey_ui::ActionTarget::Command(reference)
        );
    }

    #[test]
    fn hidden_required_input_survives_projection_refresh_and_cap_until_user_replaces_it() {
        let mut search = SearchService::new(App::new());
        for stage in [
            StartupStage::WindowAndHotkey,
            StartupStage::PersistedCatalog,
            StartupStage::AcceptQueries,
        ] {
            search.complete_stage(stage).unwrap();
        }
        let owner = PluginId("test.required-input".into());
        let item = Item {
            stable_id: ItemId("required".into()),
            plugin_id: owner.clone(),
            category: crikey_core::Category::Command,
            label: "Required input".into(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: crikey_core::ArgumentPolicy::Required,
            hit_policy: crikey_core::HitPolicy::Recorded,
            score_hint: 0,
            metadata: std::collections::BTreeMap::new(),
            actions: Vec::new(),
        };
        let reference = crikey_core::CommandRef {
            provider_id: owner.clone(),
            command_id: "required".into(),
        };
        let source_generation = Generation::from_raw(1);
        search.replace_catalog(&owner, 1, vec![item.clone()]).unwrap();
        search
            .accept_command_catalog(&owner, 1, source_generation)
            .unwrap();
        search
            .intake_modern_commands(&crikey_app::ModernCommandSnapshot {
                revision: 1,
                owners: std::collections::BTreeMap::from([(
                    owner.clone(),
                    Arc::new(crikey_app::ModernOwnerCommands {
                        request_id: 1,
                        instance: 1,
                        generation: source_generation,
                        incarnation: 1,
                        status: crikey_app::ModernCommandStatus::Ready,
                        declarations: vec![crikey_core::CommandDeclaration {
                            reference: reference.clone(),
                            label: item.label.clone(),
                            executable_target: crikey_core::ExecutableTarget::Item {
                                item_id: item.stable_id,
                                action_id: None,
                            },
                            availability: crikey_core::CommandAvailability::Available,
                            argument_policy: crikey_core::ArgumentPolicy::Required,
                        }],
                    }),
                )]),
            })
            .unwrap();
        let mut preferences = search.command_preferences_snapshot();
        Arc::make_mut(&mut preferences).preferences.records.push(
            crikey_config::command_preferences::CommandPreference {
                reference: reference.clone(),
                hidden: true,
                alias: None,
                hotkey: None,
            },
        );
        search.publish_command_preferences(preferences);
        let row = search
            .command_management_rows()
            .into_iter()
            .find(|row| row.target == crikey_ui::ActionTarget::Command(reference.clone()))
            .unwrap();
        let mut model = LauncherViewModel::new();
        model.activate();
        let mut retained = RetainedRows {
            max_results: Some(0),
            ..RetainedRows::default()
        };
        let application_owner = PluginId(APPLICATION_CATALOG_PLUGIN.into());
        let mut catalog_pipeline = QueryPipeline::new(PipelineConfig::default());
        catalog_pipeline
            .register_plugin(application_owner.clone(), application_provider_policy())
            .unwrap();
        let (_, initial_catalog) = admit_root_query(
            &mut search,
            &mut catalog_pipeline,
            &application_owner,
            &mut model,
            &mut retained,
            "prior root",
            0,
        )
        .unwrap();
        initial_catalog.expect("ordinary catalog admission before dismissal");
        model.dismiss();
        model.activate();
        model.apply(crikey_ui::UiCommand::SetQuery(item.label.clone()));
        retained.explicit_input = Some(row);
        let (generation, catalog) = admit_root_query(
            &mut search,
            &mut catalog_pipeline,
            &application_owner,
            &mut model,
            &mut retained,
            &item.label,
            1,
        )
        .unwrap();
        catalog.expect("Required activation uses the same real catalog generation admission");
        publish_root_rows(
            &search,
            None,
            &mut retained,
            &mut model,
            generation,
            &item.label,
            None,
        );
        model.apply(crikey_ui::UiCommand::ShowActions);
        publish_root_rows(
            &search,
            None,
            &mut retained,
            &mut model,
            generation,
            &item.label,
            None,
        );
        let frame = model.frame().unwrap();
        assert_eq!(frame.rows.len(), 1);
        assert_eq!(
            frame.rows[0].target,
            crikey_ui::ActionTarget::Command(reference.clone())
        );
        assert!(frame.action_panel.is_some());
        assert_eq!(
            frame.rows[0].command_state.as_ref().unwrap().argument_policy,
            crikey_core::ArgumentPolicy::Required
        );
        let (refreshed, catalog) = admit_root_query(
            &mut search,
            &mut catalog_pipeline,
            &application_owner,
            &mut model,
            &mut retained,
            &item.label,
            2,
        )
        .unwrap();
        catalog.expect("refresh after Required activation does not desynchronize intake");
        publish_root_rows(
            &search,
            None,
            &mut retained,
            &mut model,
            refreshed,
            &item.label,
            None,
        );
        assert_eq!(
            model.frame().unwrap().rows[0].target,
            crikey_ui::ActionTarget::Command(reference)
        );

        // This receipt was admitted during the native command's drain, before
        // the user's new text was applied. It must not retain presentation authority.
        let queued_receipt_intent = 42;
        let mut current_intent = queued_receipt_intent;
        assert_eq!(
            apply_native_command(
                &mut model,
                &mut retained,
                &mut current_intent,
                crikey_ui::UiCommand::SetQuery("new user query".into())
            ),
            Some(UiEffect::Query("new user query".into()))
        );
        assert_ne!(queued_receipt_intent, current_intent);
        assert!(retained.explicit_input.is_none());
        let (next, catalog) = admit_root_query(
            &mut search,
            &mut catalog_pipeline,
            &application_owner,
            &mut model,
            &mut retained,
            "new user query",
            3,
        )
        .unwrap();
        catalog.expect("next ordinary query still shares the catalog pipeline generation");
        assert!(next > refreshed);
    }

    #[test]
    fn real_dynamic_scope_survives_retirement_and_reconciles_fresh_rows_until_owner_death() {
        use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
        use std::path::PathBuf;
        use std::sync::{
            atomic::{AtomicU64, Ordering},
            Mutex,
        };

        fn wait_for<T>(stage: &str, mut observe: impl FnMut() -> Option<T>) -> T {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if let Some(value) = observe() {
                    return value;
                }
                assert!(
                    Instant::now() < deadline,
                    "native scope consumer timed out: {stage}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        struct Package(PathBuf);
        impl Drop for Package {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }

        // The same real SDK conformance worker used by provider admission tests.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join("target/native-conformance");
        let output = std::process::Command::new("cargo")
            .args(["build", "--bin", "crikey-conformance-plugin", "--manifest-path"])
            .arg(root.join("compatibility/native-conformance/Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .output()
            .expect("build native scope worker");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let executable = target
            .join("debug")
            .join(if cfg!(windows) {
                "crikey-conformance-plugin.exe"
            } else {
                "crikey-conformance-plugin"
            })
            .canonicalize()
            .unwrap();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let package_root = Package(std::env::temp_dir().join(format!(
            "crikey-cli-scope-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        let package = package_root.0.join("typed-scope-consumer");
        std::fs::create_dir_all(&package).unwrap();
        let executable = executable
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        std::fs::write(package.join("crikey.toml"), format!(
            "manifest-version = 1\n[plugin]\nid = \"typed-scope-consumer\"\nname = \"Scope consumer\"\nversion = \"0.1.0\"\nruntime = \"native\"\nentrypoint = \"{executable}\"\n"
        )).unwrap();
        std::fs::write(package.join("conformance-mode"), "typed").unwrap();
        let mut provider_pipeline = QueryPipeline::new(PipelineConfig::default());
        let provider = NativeProvider::load(
            &mut provider_pipeline,
            &[package_root.0.clone()],
            &DisabledPlugins::default(),
        );
        assert_eq!(provider.plugins().len(), 1, "{:?}", provider.unavailable());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut held = false;
        let publication_gate = Mutex::new(move |frame: &ViewModel| {
            if frame.generation == Generation::from_raw(1) && !frame.pending_plugins && !held {
                held = true;
                entered_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(60));
            }
        });
        let driver = NativeDriver::spawn(
            provider,
            provider_pipeline,
            Box::new(move |frame| publication_gate.lock().unwrap()(frame)),
        );
        let mut release = ReleaseOnDrop(Some(release_tx));
        let executor = driver.action_executor();
        let owner = driver.plugins().into_iter().next().unwrap();
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions(driver.permissions(), executor.clone())
            .unwrap();
        let mut search = ranked_service(&[]);
        search.set_plugin_action_router(Arc::new(router));
        let application_owner = PluginId(APPLICATION_CATALOG_PLUGIN.into());
        let mut catalog_pipeline = QueryPipeline::new(PipelineConfig::default());
        catalog_pipeline
            .register_plugin(application_owner.clone(), application_provider_policy())
            .unwrap();
        let mut model = LauncherViewModel::new();
        model.activate();
        model.apply(crikey_ui::UiCommand::SetQuery("typed".into()));
        let mut retained = RetainedRows::default();
        let (first, catalog) = admit_root_query(
            &mut search,
            &mut catalog_pipeline,
            &application_owner,
            &mut model,
            &mut retained,
            "typed",
            0,
        )
        .unwrap();
        catalog.unwrap();
        driver.submit(first, "typed", 0, Vec::new(), false, 0);
        let key = ExecutableKey {
            target: OwnedItem {
                owner: owner.clone(),
                item: ItemId("typed-suggestion".into()),
            },
            action: None,
        };
        let original = wait_for("first declaration", || executor.root_binding(&key, None));
        entered_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("provider held after complete publication");
        let selected = search
            .root_scope_choices()
            .into_iter()
            .find(|choice| {
                choice.identity.owner == owner
                    && choice.identity.id.0 == "secure"
                    && matches!(
                        choice.identity.source,
                        crikey_ui::PublicationIdentity::Suggestion { .. }
                    )
            })
            .expect("suggestion-only scope");
        publish_projected_rows(&search, &retained, &mut model, first);
        let Some(UiEffect::Query(query)) =
            model.apply(crikey_ui::UiCommand::SetScope(Some(selected.identity.clone())))
        else {
            panic!("scope selection must request a new admitted query");
        };
        let scope = crikey_app::root_scope_from_identity(&selected.identity);
        let (next, catalog) = admit_root_query(
            &mut search,
            &mut catalog_pipeline,
            &application_owner,
            &mut model,
            &mut retained,
            &query,
            1,
        )
        .unwrap();
        catalog.unwrap();
        driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: next,
                    text: &query,
                    now: 1,
                    selected: 0,
                },
                Vec::new(),
                false,
                Some(scope.clone()),
            )
            .unwrap();
        retained.mark_pending(next, RowSource::Native, true);
        assert!(
            executor.root_binding(&key, None).is_none(),
            "old executable admission must retire"
        );
        publish_projected_rows(&search, &retained, &mut model, next);
        let pending = model.frame().unwrap();
        assert_eq!(
            pending.navigation.selected_scope.as_ref(),
            Some(&selected.identity)
        );
        assert!(pending.navigation.unavailable.is_none());
        assert!(pending
            .navigation
            .scopes
            .iter()
            .any(|choice| choice.identity == selected.identity));
        assert!(pending.rows.is_empty());

        release.0.take().unwrap().send(()).unwrap();
        let outcome = wait_for("fresh scoped rows", || {
            driver
                .take_outcome()
                .filter(|frame| frame.generation == next && !frame.pending_plugins)
        });
        let fresh = executor
            .root_binding(&key, None)
            .expect("fresh executable admission");
        assert_ne!(fresh.source, original.source);
        retained.absorb(next, RowSource::Native, &outcome.rows, outcome.pending_plugins);
        publish_projected_rows(&search, &retained, &mut model, next);
        let current = model.frame().unwrap();
        assert_eq!(
            current.navigation.selected_scope.as_ref(),
            Some(&selected.identity)
        );
        assert!(current.navigation.unavailable.is_none());
        assert!(current
            .navigation
            .scopes
            .iter()
            .any(|choice| choice.identity == selected.identity));
        assert!(current.rows.iter().any(|row| matches!(&row.target,
            crikey_ui::ActionTarget::Result { owner: row_owner, item } if row_owner == &owner && item.0 == "typed-suggestion")));

        drop(driver);
        assert!(!executor.root_scope_current(&scope));
        publish_projected_rows(&search, &retained, &mut model, next);
        let retired = model.frame().unwrap();
        assert_eq!(
            retired.navigation.selected_scope.as_ref(),
            Some(&selected.identity)
        );
        assert!(retired
            .navigation
            .unavailable
            .as_deref()
            .is_some_and(|notice| notice.contains("Selected scope is unavailable")));
        assert!(retired.rows.is_empty());
        assert!(!retired
            .navigation
            .scopes
            .iter()
            .any(|choice| choice.identity == selected.identity));
    }
}
