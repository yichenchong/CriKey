//! A loaded legacy plugin's suggestions must traverse the app's
//! [`QueryPipeline`] (spec 7.1, 14.3, 14.5; acceptance 31.9, 31.14 - 31.18;
//! roadmap M3).
//!
//! This is the M3 counterpart of the M2 regression
//! `built_in_application_results_cross_intake_before_prompt_publication` in
//! `crikey-cli`: it proves that the *legacy* provider is wired into the live
//! query path and not merely reachable from the `crikey dev` commands. A real
//! child CPython interpreter runs the `well-behaved` fixture's `on_suggest`, and
//! the item it publishes is shown to cross the pipeline's bounded intake and
//! come back out on the presented frame under the current generation.
//!
//! # Two boundaries, one proof
//!
//! [`legacy_suggestions_cross_pipeline_intake_before_presentation`] drives the
//! provider directly, keeping the pipeline in hand so its bounded intake can be
//! inspected: the legacy batch is admitted, merged, and presented under the
//! current generation.
//! [`legacy_supervisor_publishes_off_the_ui_thread`] then drives the same plugin
//! through the live app path — the [`LegacyDriver`] supervisor — and shows the
//! merged frame arriving *asynchronously*, published under the generation it was
//! submitted with rather than a newer one (Finding 8). Deleting the legacy
//! registration or the drive in either path leaves no `legacy.well-behaved` row
//! and fails the run.
//!
//! # A missing interpreter is a failure, not a skip
//!
//! The M3 suite's standing rule, honoured by every test file, is that a host
//! with no supported CPython fails loudly rather than skipping (Finding 9): the
//! interpreter-search error quotes the interpreters it looked for, so the
//! failure is diagnosable rather than a silent green run.
//!
//! # Isolation and timing
//!
//! The `on_suggest` callback runs in a separate operating-system process, never
//! in the test process. Scheduling time is the explicit virtual millisecond the
//! test advances by hand; the only wall-clock bounds are the worker's startup
//! and per-call budgets, which are values the provider passes in, and the
//! bounded poll below that waits for the out-of-process answer.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use crikey_app::{LegacyDriver, LegacyProvider, PipelineConfig, QueryPipeline};
use crikey_core::{Generation, PluginId};
use crikey_legacy_compat::{discover_interpreter, LegacyDeadlines};
use crikey_python_host::RuntimeProfile;
use crikey_ui::ViewModel;

/// Directory that holds the version-controlled synthetic legacy test plugins,
/// which discovery treats as one package root (spec 14.3).
fn test_plugins_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives two directories below the workspace root")
        .join("compatibility/test-plugins")
}

/// Fails loudly, never skips, when this host has no supported CPython: the
/// interpreter-search error is quoted so the failure is diagnosable (Finding 9).
fn require_legacy_interpreter() {
    if let Err(error) = discover_interpreter(&RuntimeProfile::LegacyCompatibility) {
        panic!("the legacy compatibility suite requires a supported CPython on this host: {error}");
    }
}

#[test]
fn legacy_suggestions_cross_pipeline_intake_before_presentation() {
    require_legacy_interpreter();

    let well_behaved = PluginId("legacy.well-behaved".to_owned());
    let cache_root = std::env::temp_dir().join("crikey-legacy-provider-test-cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = LegacyProvider::load(
        &mut pipeline,
        &[test_plugins_root()],
        cache_root,
        LegacyDeadlines::default(),
    );

    // Discovery honours skip-don't-fail: it must have loaded the well-behaved
    // fixture and registered it with the pipeline, whatever it made of the
    // other packages in the root.
    assert!(
        provider.plugins().contains(&well_behaved),
        "the well-behaved legacy plugin must load and register; unavailable: {:?}",
        provider.unavailable(),
    );

    // A query typed into the app dispatches to the legacy plugin, whose child
    // process answers `on_suggest`, and the item crosses the pipeline.
    let frame = provider
        .drive_query(&mut pipeline, "report", 17)
        .expect("the admitted current legacy batch produces a frame");

    // The frame belongs to the query's generation, and it is the pipeline's
    // currently visible one: a stale answer would have been refused at intake
    // rather than shown here (acceptance 31.7).
    assert_eq!(
        pipeline.visible_generation(),
        Some(frame.generation),
        "the presented frame is the current generation",
    );

    // The legacy item genuinely traversed the bounded intake: it was admitted
    // and merged, exactly as the built-in application provider's batch is.
    assert!(
        pipeline.intake_diagnostics().admitted() >= 1,
        "the legacy batch must be admitted to intake",
    );
    assert!(
        pipeline.intake_diagnostics().merged() >= 1,
        "the legacy batch must be merged out of intake",
    );

    // And it is present on the frame, owned by the legacy plugin.
    assert!(
        frame.rows.iter().any(|row| row.plugin_name == well_behaved.0),
        "the presented frame must carry the legacy plugin's suggestion, found: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );

    provider.shutdown(180);
}

#[test]
fn legacy_supervisor_publishes_off_the_ui_thread() {
    require_legacy_interpreter();

    let well_behaved = PluginId("legacy.well-behaved".to_owned());
    let cache_root = std::env::temp_dir().join("crikey-legacy-supervisor-test-cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = LegacyProvider::load(
        &mut pipeline,
        &[test_plugins_root()],
        cache_root,
        LegacyDeadlines::default(),
    );
    assert!(
        provider.plugins().contains(&well_behaved),
        "the well-behaved legacy plugin must load and register; unavailable: {:?}",
        provider.unavailable(),
    );

    // The live app path drives the plugin off the UI thread. The supervisor
    // publishes each merged frame through this callback, exactly as the launcher
    // forwards it to the renderer.
    let published: Arc<Mutex<Option<ViewModel>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&published);
    let driver = LegacyDriver::spawn(provider, pipeline, move |frame| {
        *sink.lock().expect("the publish sink is not poisoned") = Some(frame.clone());
    });
    assert!(
        driver.has_plugins(),
        "the supervisor must serve the loaded legacy plugin",
    );

    // Submitting returns immediately: the UI thread never blocks on the child
    // interpreter (spec 6.5, acceptance 31.1). The merged frame is tagged with
    // this generation and must never surface under a newer one (Finding 8).
    let generation = Generation::from_raw(1);
    driver.submit(generation, "report", 17, Vec::new(), false, 0);

    // Poll for the asynchronous publish with a bounded budget: this only waits
    // for the out-of-process answer, it does not busy-block the caller.
    let mut frame = None;
    for _ in 0..2_000 {
        if let Some(candidate) = published
            .lock()
            .expect("the publish sink is not poisoned")
            .clone()
        {
            frame = Some(candidate);
            break;
        }
        sleep(Duration::from_millis(5));
    }
    let frame = frame.expect("the supervisor must publish a legacy frame off the UI thread");

    assert_eq!(
        frame.generation, generation,
        "the async frame is published under the generation it was submitted with, never a newer one",
    );
    assert!(
        frame.rows.iter().any(|row| row.plugin_name == well_behaved.0),
        "the async frame must carry the legacy plugin's suggestion, found: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );

    // Dropping the driver signals shutdown and joins the supervisor thread,
    // tearing the child down cleanly — no thread or process leak.
    drop(driver);
}
