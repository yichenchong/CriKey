//! `crikey` command-line entrypoint (spec 28).

mod activation_commands;
mod dev_commands;
mod legacy_commands;
mod modern_commands;
mod native_commands;
mod package_commands;

use crikey_app::{
    admitted_plugin_roots, ActionSubmission, App, BatchState, LegacyDriver, LegacyProvider, ModernDriver,
    ModernProvider, NativeDriver, NativeProvider, PipelineConfig, PluginActionRouter, QueryPipeline,
    ResultBatch, SearchService, StartupJournal, StartupMode, StartupStage,
};
use crikey_benchmarks::{
    run_catalog_benchmark, BenchmarkConfig, BenchmarkReport, PrefixLatency, STRESS_CATALOG_SIZE,
};
use crikey_catalog::{CatalogCache, FileCatalogCache};
use crikey_core::{Generation, Item, PluginId};
use crikey_input_scheduler::{
    ActivationPolicy, DebouncePolicy, PluginPolicy, QueuePolicy, SchedulingProfile,
};
use crikey_legacy_compat::LegacyDeadlines;
use crikey_ui::{
    LauncherViewModel, NativeLauncher, NativeLauncherConfig, NativeLauncherEvent, UiEffect, ViewModel,
};
use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

const USAGE: &str = "\
crikey - a fast, keyboard-driven application launcher

USAGE:
    crikey <COMMAND> [ARGS]

COMMANDS:
    run                             Start the launcher (use `crikey run --help`)
    plugin                          Plugin management (not available yet)
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
#[cfg(windows)]
const DEFAULT_ACTIVATION_HOTKEY: &str = "Ctrl+Alt+Space";

const RUN_USAGE: &str = "\
crikey run - start the launcher

USAGE:
    crikey run
    crikey run --help

OPTIONS:
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    dispatch(&args)
}

fn dispatch(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
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
                run_launcher(&args[1..])
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
        Some("plugin") => {
            eprintln!("crikey: `plugin` is not available yet");
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
    if let Some(error) = search.catalog_cache_error() {
        eprintln!("crikey: application catalog cache write failed: {error}");
    }
    search
        .complete_stage(StartupStage::PersistedCatalog)
        .map_err(|error| error.to_string())?;
    search
        .complete_stage(StartupStage::AcceptQueries)
        .map_err(|error| error.to_string())?;
    // SearchService remains the synchronous matcher and ranker. Its actual
    // result items still cross the M2 intake and presentation boundary before
    // its richer ranked/highlighted rows become visible.
    let mut query_pipeline = QueryPipeline::new(PipelineConfig::default());
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
    let mut legacy_pipeline = QueryPipeline::new(PipelineConfig::default());
    let legacy_cache_root = legacy_cache_root()?;
    let legacy_provider = LegacyProvider::load(
        &mut legacy_pipeline,
        &admitted_plugin_roots(&startup_mode, &legacy_package_roots()),
        legacy_cache_root,
        LegacyDeadlines::default(),
    );
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
    // Legacy plugins run out-of-process on a dedicated supervisor thread so a
    // slow child interpreter can never block the UI thread (Finding 8; spec
    // 6.5, acceptance 31.1, 31.8). The supervisor publishes each merged frame
    // straight to the renderer through this cloned handle; the UI thread folds
    // the same frame into its retained view model on its next turn.
    let legacy_publish_handle = render_handle.clone();
    let legacy_driver = LegacyDriver::spawn(legacy_provider, legacy_pipeline, move |frame| {
        let _ = legacy_publish_handle.submit_frame(frame);
    });

    // Modern python plugins join the same live query path, driven off the UI
    // thread by their own supervisor (spec 15.6; acceptance 31.10). Discovery
    // scans `CRIKEY_MODERN_PLUGIN_ROOTS`; an unset variable loads nothing, so
    // `crikey run` behaves exactly as before on a host with no modern plugins.
    // A crashing interpreter degrades to a recorded diagnostic and never aborts
    // the process (contract §8), so no load failure here is fatal to the launch.
    let mut modern_pipeline = QueryPipeline::new(PipelineConfig::default());
    let cache_root = modern_cache_root()?;
    let mut modern_provider = ModernProvider::load(
        &mut modern_pipeline,
        &admitted_plugin_roots(&startup_mode, &modern_plugin_roots()),
        modern_index_root(),
        cache_root,
    );
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
    // The supervisor publishes each merged frame straight to the renderer,
    // mirroring the legacy driver above; both run off the UI thread so a slow
    // or dead child interpreter can never block it.
    let modern_publish_handle = render_handle.clone();
    let modern_driver = ModernDriver::spawn(modern_provider, modern_pipeline, move |frame| {
        let _ = modern_publish_handle.submit_frame(frame);
    });

    // Native plugins use the same asynchronous query boundary as the legacy
    // and modern providers. Discovery is intentionally empty unless the
    // operator names native package roots (spec 16.1, 16.6).
    let mut native_pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut native_provider = NativeProvider::load(
        &mut native_pipeline,
        &admitted_plugin_roots(&startup_mode, &native_plugin_roots()),
    );
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
        Box::new(move |frame| {
            let _ = native_publish_handle.submit_frame(frame);
        }),
    );
    // Plugin-owned actions use the same exact-owner endpoints and budget
    // handles retained by the provider drivers. Registering this router before
    // the event loop makes `crikey run` execute selected modern/native actions
    // instead of falling through to host launch handling.
    let mut action_router = PluginActionRouter::default();
    action_router
        .register(modern_driver.plugins(), modern_driver.action_executor())
        .map_err(|error| format!("cannot register modern action runtime: {error}"))?;
    action_router
        .register(native_driver.plugins(), native_driver.action_executor())
        .map_err(|error| format!("cannot register native action runtime: {error}"))?;
    search.set_plugin_action_router(Arc::new(action_router));
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
    let mut retained = RetainedRows::default();
    let outcome = launcher
        .run(ready_on_first_event(Rc::clone(&ledger), move |event| {
            // Fold any legacy rows the supervisor produced since the last turn
            for completion in search.poll_action_completions() {
                let message = match completion.outcome {
                    Ok(()) => format!(
                        "Action completed: {} / {}",
                        completion.plugin.0, completion.action_id.0
                    ),
                    Err(error) => format!(
                        "Action failed ({} / {}): {error}",
                        completion.plugin.0, completion.action_id.0
                    ),
                };
                eprintln!("crikey: {message}");
                view_model.set_selected_status(message);
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
                view_model.publish(outcome.generation, retained.merged(), retained.pending());
            }
            // Fold the modern supervisor's rows the same way. Both drivers
            // publish the built-in rows ahead of their own; merging by source
            // keeps legacy and modern rows coexisting instead of the later fold
            // clobbering the earlier (contract §8).
            if let Some(outcome) = modern_driver.take_outcome() {
                retained.absorb(
                    outcome.generation,
                    RowSource::Modern,
                    &outcome.rows,
                    outcome.pending_plugins,
                );
                view_model.publish(outcome.generation, retained.merged(), retained.pending());
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
                view_model.publish(outcome.generation, retained.merged(), retained.pending());
            }
            // Catalog outcomes use the same SearchService instance/owner
            // publication edge as persisted slices. Obsolete and failed
            // results are observable but can never replace live state.
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
                        let items = search.results().iter().map(|hit| hit.item.clone()).collect();
                        view_model.begin_generation(generation);

                        let now = u64::try_from(query_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
                        // The built-in provider stays synchronous: it is fast,
                        // in-process, and its rows must reach this frame without
                        // waiting on anything. Its pipeline generation stays in
                        // lockstep with the search generation this frame is
                        // published under.
                        let application_frame =
                            drive_application_provider(&mut query_pipeline, &owner, &raw, items, now);
                        if let Some(frame) = application_frame {
                            if frame.generation == generation {
                                let rows = search.result_rows();
                                // Legacy plugins are driven off the UI thread
                                // (Finding 8): hand the query to the supervisor
                                // and return. The built-in rows publish now, with
                                // work marked pending while a legacy answer is
                                // outstanding; the legacy rows fold in
                                // asynchronously through `take_outcome` and the
                                // supervisor's own frame submission, always under
                                // this generation (spec 14.5).
                                let legacy_outstanding = legacy_driver.has_plugins();
                                if legacy_outstanding {
                                    legacy_driver.submit(
                                        generation,
                                        &raw,
                                        now,
                                        rows.clone(),
                                        frame.pending_plugins,
                                        0,
                                    );
                                }
                                // Modern plugins are dispatched off the UI thread
                                // just like legacy ones; their supervisor folds
                                // its rows into the renderer under this same
                                // generation as they arrive.
                                let modern_outstanding = modern_driver.has_plugins();
                                if modern_outstanding {
                                    modern_driver.submit(
                                        generation,
                                        &raw,
                                        now,
                                        rows.clone(),
                                        frame.pending_plugins,
                                        0,
                                    );
                                }
                                // Native plugins are dispatched through their
                                // own supervisor and merged on the same
                                // generation as the built-in rows.
                                let native_outstanding = native_driver.has_plugins();
                                if native_outstanding {
                                    native_driver.submit(
                                        generation,
                                        &raw,
                                        now,
                                        rows.clone(),
                                        frame.pending_plugins,
                                        0,
                                    );
                                }
                                retained.set_builtin(generation, rows, frame.pending_plugins);
                                retained.mark_pending(generation, RowSource::Legacy, legacy_outstanding);
                                retained.mark_pending(generation, RowSource::Native, native_outstanding);
                                retained.mark_pending(generation, RowSource::Modern, modern_outstanding);
                                view_model.publish(generation, retained.merged(), retained.pending());
                            }
                        }
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
                    Ok(ActionSubmission::Completed) => {
                        view_model.dismiss();
                        if let Some(session) = command_session {
                            let _ = render_handle.request_hide_session(session);
                        }
                        if !has_activation_source {
                            let _ = render_handle.request_exit();
                        }
                    }
                    Ok(ActionSubmission::Pending(request_id)) => {
                        let message = format!(
                            "Action pending ({} / request {})",
                            request_id.plugin.0, request_id.sequence
                        );
                        view_model.set_selected_status(message.clone());
                        eprintln!("crikey: {message}");
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
        }))
        .map_err(|error| error.to_string());
    // Only a deliberate exit clears the record; a run that ended in an error
    // leaves its plugin set on disk for the next launch to read.
    if outcome.is_ok() {
        ledger.borrow_mut().mark_clean_shutdown();
    }
    outcome
}

/// Directories scanned for legacy packages on the live path (spec 14.3).
///
/// Read from `CRIKEY_LEGACY_PACKAGE_ROOTS` using the platform path-list syntax,
/// so an operator can point `crikey run` at their installed packages today.
/// Resolving roots from the settings file (spec 14.7) is left to a later
/// milestone; an unset variable means no legacy roots, which loads nothing
/// rather than failing.
fn legacy_package_roots() -> Vec<std::path::PathBuf> {
    match std::env::var_os("CRIKEY_LEGACY_PACKAGE_ROOTS") {
        Some(value) => std::env::split_paths(&value).collect(),
        None => Vec::new(),
    }
}

/// Directories scanned for modern python plugins on the live path (spec 15.1).
///
/// Read from `CRIKEY_MODERN_PLUGIN_ROOTS` using the platform path-list syntax,
/// mirroring [`legacy_package_roots`]. Each root holds `<id>/crikey.toml` plugin
/// subdirectories (contract §11). An unset variable means no modern roots, which
/// loads nothing rather than failing — `crikey run` is unchanged on a host with
/// none.
fn modern_plugin_roots() -> Vec<std::path::PathBuf> {
    match std::env::var_os("CRIKEY_MODERN_PLUGIN_ROOTS") {
        Some(value) => std::env::split_paths(&value).collect(),
        None => Vec::new(),
    }
}

/// Directories scanned for native packages on the live path (spec 16.1).
///
/// The native provider performs manifest and platform/architecture filtering;
/// this helper only applies the platform path-list syntax and keeps an unset
/// variable equivalent to an empty discovery set.
fn native_plugin_roots() -> Vec<std::path::PathBuf> {
    match std::env::var_os("CRIKEY_NATIVE_PLUGIN_ROOTS") {
        Some(value) => std::env::split_paths(&value).collect(),
        None => Vec::new(),
    }
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
    std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".local").join("state"))
        })
        .map(|base| base.join("crikey").join("startup.json"))
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
fn catalog_cache_root() -> Result<std::path::PathBuf, String> {
    if let Some(value) = std::env::var_os("CRIKEY_CATALOG_CACHE_ROOT").filter(|value| !value.is_empty()) {
        let path = std::path::PathBuf::from(value);
        create_private_dir(&path)?;
        return Ok(path);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .ok_or_else(|| {
            "cannot determine a per-user cache directory for the search catalog: set \
             CRIKEY_CATALOG_CACHE_ROOT, XDG_CACHE_HOME or HOME (refusing to use a \
             world-writable shared temporary directory as a trust root)"
                .to_owned()
        })?;
    let path = base.join("crikey").join("catalog");
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
        return Ok(std::path::PathBuf::from(value));
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .ok_or_else(|| {
            "cannot determine a per-user cache directory for modern plugins: set \
             CRIKEY_MODERN_CACHE_ROOT, XDG_CACHE_HOME or HOME (refusing to use a \
             world-writable shared temporary directory as a trust root)"
                .to_owned()
        })?;
    let dir = base.join("crikey").join("modern");
    create_private_dir(&dir)?;
    Ok(dir)
}
/// The legacy archive extraction root, read from `CRIKEY_LEGACY_CACHE_ROOT`.
///
/// The loader trusts an existing content-addressed extraction directory, so a
/// shared temporary directory would let another local process plant plugin
/// files before the child interpreter imports them. The default is per-user
/// and restricted to this account, just like the modern environment cache.
fn legacy_cache_root() -> Result<std::path::PathBuf, String> {
    if let Some(value) = std::env::var_os("CRIKEY_LEGACY_CACHE_ROOT").filter(|value| !value.is_empty()) {
        let path = std::path::PathBuf::from(value);
        create_private_dir(&path)?;
        return Ok(path);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .ok_or_else(|| {
            "cannot determine a per-user cache directory for legacy packages: set \
             CRIKEY_LEGACY_CACHE_ROOT, XDG_CACHE_HOME or HOME (refusing to use a \
             world-writable shared temporary directory as a trust root)"
                .to_owned()
        })?;
    let path = base.join("crikey").join("legacy");
    create_private_dir(&path)?;
    Ok(path)
}

/// Creates `dir` (and parents) as a private, per-user directory. On unix the
/// leaf is forced to `0700` so a cache later imported by path can never be
/// world- or group-writable. If the directory already exists but is not ours,
/// forcing the mode fails and the caller refuses rather than trust it.
fn create_private_dir(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| {
        format!(
            "cannot create private cache directory `{}`: {error}",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!(
                "cannot secure private cache directory `{}`: {error}",
                dir.display()
            )
        })?;
    }
    Ok(())
}

/// The provider group a published row belongs to, used to merge the built-in,
/// legacy, modern and native row sets into one presented frame without one
/// clobbering another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowSource {
    Builtin,
    Legacy,
    Modern,
    Native,
}

/// Classifies a published row by the source group its owning plugin belongs to
/// (spec 10.2 namespacing): `legacy.*`, `modern.*` and `native.*` are the three
/// asynchronous providers; everything else — the built-in application catalog
/// included — is built-in.
fn row_source(plugin_name: &str) -> RowSource {
    if plugin_name.starts_with("legacy.") {
        RowSource::Legacy
    } else if plugin_name.starts_with("modern.") {
        RowSource::Modern
    } else if plugin_name.starts_with("native.") {
        RowSource::Native
    } else {
        RowSource::Builtin
    }
}

/// The presented row set kept between UI turns, grouped by source so folding one
/// provider's asynchronous outcome never clobbers another's rows.
///
/// Each asynchronous provider publishes the built-in rows ahead of its own,
/// and each calls back independently through `take_outcome`; without per-source
/// retention the second fold of a turn would overwrite the first provider's
/// rows. Keeping the four groups and re-merging on every fold makes legacy,
/// modern and native rows coexist under one generation (contract §8). A new
/// generation drops every retained group first, so stale rows never cross a
/// query boundary.
#[derive(Debug, Default)]
struct RetainedRows {
    generation: Option<Generation>,
    builtin: Vec<crikey_ui::ResultRow>,
    legacy: Vec<crikey_ui::ResultRow>,
    modern: Vec<crikey_ui::ResultRow>,
    native: Vec<crikey_ui::ResultRow>,
    builtin_pending: bool,
    legacy_pending: bool,
    modern_pending: bool,
    native_pending: bool,
}

impl RetainedRows {
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
                    ..RetainedRows::default()
                };
                true
            }
        }
    }

    /// Refreshes the built-in group from the synchronous publish.
    fn set_builtin(&mut self, generation: Generation, rows: Vec<crikey_ui::ResultRow>, pending: bool) {
        if !self.begin(generation) {
            return;
        }
        self.builtin = rows;
        self.builtin_pending = pending;
    }

    /// Records that a provider's answer is still outstanding for `generation`, so
    /// the merged frame stays marked pending until that outcome folds in.
    fn mark_pending(&mut self, generation: Generation, source: RowSource, pending: bool) {
        if !self.begin(generation) {
            return;
        }
        match source {
            RowSource::Builtin => self.builtin_pending = pending,
            RowSource::Legacy => self.legacy_pending = pending,
            RowSource::Modern => self.modern_pending = pending,
            RowSource::Native => self.native_pending = pending,
        }
    }

    /// Folds one provider's outcome. `rows` are the built-in rows followed by
    /// `source`'s own rows; only the built-in group and `source`'s group are
    /// refreshed, so a sibling provider's rows survive this fold. A stale
    /// (already-superseded) outcome is ignored.
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
        self.builtin = rows
            .iter()
            .filter(|row| row_source(&row.plugin_name) == RowSource::Builtin)
            .cloned()
            .collect();
        match source {
            RowSource::Builtin => self.builtin_pending = pending,
            RowSource::Legacy => {
                self.legacy = rows
                    .iter()
                    .filter(|row| row_source(&row.plugin_name) == RowSource::Legacy)
                    .cloned()
                    .collect();
                self.legacy_pending = pending;
            }
            RowSource::Modern => {
                self.modern = rows
                    .iter()
                    .filter(|row| row_source(&row.plugin_name) == RowSource::Modern)
                    .cloned()
                    .collect();
                self.modern_pending = pending;
            }
            RowSource::Native => {
                self.native = rows
                    .iter()
                    .filter(|row| row_source(&row.plugin_name) == RowSource::Native)
                    .cloned()
                    .collect();
                self.native_pending = pending;
            }
        }
    }
    /// The merged presentation order: built-in rows, then legacy, modern and native.
    fn merged(&self) -> Vec<crikey_ui::ResultRow> {
        let mut rows = Vec::with_capacity(
            self.builtin.len() + self.legacy.len() + self.modern.len() + self.native.len(),
        );
        rows.extend(self.builtin.iter().cloned());
        rows.extend(self.legacy.iter().cloned());
        rows.extend(self.modern.iter().cloned());
        rows.extend(self.native.iter().cloned());
        rows
    }

    /// Whether any source still has work outstanding for the retained generation.
    fn pending(&self) -> bool {
        self.builtin_pending || self.legacy_pending || self.modern_pending || self.native_pending
    }
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
        },
        max_concurrent_requests: 1,
        queue_policy: QueuePolicy::ReplaceOldest,
        queue_capacity: 1,
    }
}

fn drive_application_provider(
    pipeline: &mut QueryPipeline,
    owner: &PluginId,
    query: &str,
    items: Vec<Item>,
    now: u64,
) -> Option<ViewModel> {
    let generation = pipeline.keystroke(query, now);
    let tick = pipeline.tick(now);
    let tick_succeeded = tick.errors.is_empty();

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
        delivered_current = pipeline
            .deliver(
                ResultBatch {
                    generation: request.generation,
                    plugin: request.plugin.clone(),
                    state: BatchState::Final,
                    items: batch_items,
                },
                now,
            )
            .is_ok();
        let _ = pipeline.complete(&request.plugin, request.generation, now);
    }

    let frame = pipeline.present(now);
    let presentation_succeeded = pipeline.take_errors().is_empty();
    if !tick_succeeded || !delivered_current || !presentation_succeeded {
        return None;
    }
    frame.filter(|frame| frame.generation == generation)
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
        let expected_ids = search_rows.iter().map(|row| row.item.clone()).collect::<Vec<_>>();
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

        let pipeline_ids = frame.rows.iter().map(|row| row.item.clone()).collect::<Vec<_>>();
        let published_ids = published
            .rows
            .iter()
            .map(|row| row.item.clone())
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

    fn row(plugin: &str, label: &str) -> crikey_ui::ResultRow {
        crikey_ui::ResultRow {
            item: crikey_core::ItemId(label.to_owned()),
            label: label.to_owned(),
            description: String::new(),
            icon_reference: None,
            category: String::new(),
            plugin_name: plugin.to_owned(),
            highlights: Vec::new(),
            argument_hint: None,
            status: None,
            default_action: None,
            alternate_actions: Vec::new(),
        }
    }

    #[test]
    fn retained_rows_merge_legacy_and_modern_by_source() {
        let generation = Generation::from_raw(7);
        let mut retained = RetainedRows::default();

        // The synchronous built-in publish, with both async providers still out.
        retained.set_builtin(generation, vec![row("builtin.app", "app")], false);
        retained.mark_pending(generation, RowSource::Legacy, true);
        retained.mark_pending(generation, RowSource::Modern, true);
        assert!(retained.pending(), "outstanding providers keep the frame pending");

        // The legacy supervisor answers first with built-in + legacy rows.
        retained.absorb(
            generation,
            RowSource::Legacy,
            &[row("builtin.app", "app"), row("legacy.files", "file")],
            false,
        );
        // Then the modern supervisor answers with built-in + modern rows. A
        // merge that replaced the whole frame would drop the legacy row here.
        retained.absorb(
            generation,
            RowSource::Modern,
            &[row("builtin.app", "app"), row("modern.web", "web")],
            false,
        );

        let sources: Vec<String> = retained.merged().iter().map(|r| r.plugin_name.clone()).collect();
        // Kills the "each supervisor's publish clobbers the other" mutation:
        // whole-frame replacement would leave only built-in + `modern.web`.
        assert_eq!(
            sources,
            vec!["builtin.app", "legacy.files", "modern.web"],
            "built-in, legacy and modern rows coexist in that presentation order",
        );
        assert!(
            !retained.pending(),
            "once every source has answered the merged frame is no longer pending",
        );
    }

    #[test]
    fn retained_rows_drop_a_superseded_generation() {
        let old = Generation::from_raw(1);
        let new = Generation::from_raw(2);
        let mut retained = RetainedRows::default();

        retained.absorb(old, RowSource::Legacy, &[row("legacy.old", "x")], false);
        // A newer generation's first fold drops every stale group first, so an
        // older generation's rows never cross the boundary. Kills the "reuse
        // retained rows across generations" mutation.
        retained.absorb(new, RowSource::Modern, &[row("modern.new", "y")], false);

        let sources: Vec<String> = retained.merged().iter().map(|r| r.plugin_name.clone()).collect();
        assert_eq!(
            sources,
            vec!["modern.new"],
            "stale legacy rows from an older generation are dropped",
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
        assert_eq!(
            *seen.borrow(),
            vec![NativeLauncherEvent::Activated],
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
}
