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

use crikey_app::{
    CatalogBuildResult, DisabledPlugins, LegacyDirectories, LegacyDriver, LegacyProvider, PipelineConfig,
    PluginActionRouter, QueryPipeline,
};
use crikey_core::{ExecutionPolicy, Generation, PluginId};
use crikey_legacy_compat::{discover_interpreter, LegacyDeadlines};
use crikey_plugin_supervisor::BudgetKind;
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
        LegacyDirectories::default(),
        LegacyDeadlines::default(),
        &DisabledPlugins::default(),
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
        LegacyDirectories::default(),
        LegacyDeadlines::default(),
        &DisabledPlugins::default(),
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

#[test]
fn legacy_driver_rejects_a_delayed_older_generation() {
    require_legacy_interpreter();

    let well_behaved = PluginId("legacy.well-behaved".to_owned());
    let cache_root = std::env::temp_dir().join("crikey-legacy-generation-test-cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = LegacyProvider::load(
        &mut pipeline,
        &[test_plugins_root()],
        cache_root,
        LegacyDirectories::default(),
        LegacyDeadlines::default(),
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&well_behaved),
        "the well-behaved legacy plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let published: Arc<Mutex<Vec<Generation>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let driver = LegacyDriver::spawn(provider, pipeline, move |frame| {
        sink.lock()
            .expect("the publish sink is not poisoned")
            .push(frame.generation);
    });

    let older = Generation::from_raw(1);
    let newer = Generation::from_raw(2);
    driver.submit(older, "report old", 17, Vec::new(), false, 0);
    driver.submit(newer, "report new", 18, Vec::new(), false, 0);

    let mut seen_newer = false;
    for _ in 0..2_000 {
        if published
            .lock()
            .expect("the publish sink is not poisoned")
            .contains(&newer)
        {
            seen_newer = true;
            break;
        }
        sleep(Duration::from_millis(5));
    }
    assert!(seen_newer, "the newer generation must be presented");

    // A stale caller can arrive after the newer frame is visible. It must not
    // rewind `current` or publish another legacy frame under generation 1.
    driver.submit(older, "report old again", 19, Vec::new(), false, 0);
    for _ in 0..200 {
        if published
            .lock()
            .expect("the publish sink is not poisoned")
            .iter()
            .any(|generation| *generation == older)
        {
            break;
        }
        sleep(Duration::from_millis(5));
    }
    let generations = published
        .lock()
        .expect("the publish sink is not poisoned")
        .clone();
    assert!(
        generations.iter().all(|generation| *generation == newer),
        "a delayed older submission must not rewind publication; saw {generations:?}",
    );

    drop(driver);
}

/// The catalog gap this suite exists to close: `LegacyRuntime::catalog_rebuild`
/// was implemented and never called from the launcher, so a legacy plugin
/// published nothing searchable in `crikey run` (spec 14.8).
///
/// The rebuild is admitted the way the composition root admits it and then
/// runs on the supervisor thread, because `on_catalog` executes in the child
/// interpreter. Deleting the `drive_catalogs` call in `LegacyDriver::spawn`
/// leaves this test with no catalog result at all.
#[test]
fn a_legacy_plugins_catalog_reaches_the_live_driver() {
    require_legacy_interpreter();

    let well_behaved = PluginId("legacy.well-behaved".to_owned());
    let cache_root = std::env::temp_dir().join("crikey-legacy-catalog-test-cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = LegacyProvider::load(
        &mut pipeline,
        &[test_plugins_root()],
        cache_root,
        LegacyDirectories::default(),
        LegacyDeadlines::default(),
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&well_behaved),
        "the well-behaved legacy plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );
    provider
        .request_catalog_build(&well_behaved, 1, Generation::ZERO)
        .expect("a loaded legacy plugin admits its first catalog build");

    let driver = LegacyDriver::spawn(provider, pipeline, |_frame| {});

    let mut results = Vec::new();
    for _ in 0..2_000 {
        results = driver.take_catalog_results();
        if !results.is_empty() {
            break;
        }
        sleep(Duration::from_millis(5));
    }
    let result = results
        .pop()
        .expect("the supervisor must run the admitted catalog build");
    let CatalogBuildResult::Complete(build) = result else {
        panic!("the well-behaved fixture publishes a catalog, got {result:?}");
    };

    assert_eq!(build.plugin, well_behaved);
    assert_eq!(build.instance, 1);
    // `data/catalog.txt` is the committed resource `on_catalog` reads, so the
    // labels prove the callback genuinely ran in the child rather than the host
    // inventing an empty catalog.
    let labels = build
        .items
        .iter()
        .map(|item| item.label.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        vec![
            "Well Behaved Alpha".to_owned(),
            "Well Behaved Beta".to_owned(),
            "Well Behaved Gamma".to_owned(),
        ],
        "the catalog the plugin published must reach the launcher intact",
    );
    // A catalog row that carried no action could never be launched, so the
    // host attaches the one action the legacy contract defines.
    assert!(
        build.items.iter().all(|item| item
            .actions
            .iter()
            .any(|action| action.execution_policy == ExecutionPolicy::Plugin)),
        "every legacy catalog item must carry its plugin-owned default action",
    );

    drop(driver);
}

/// The action gap: a legacy row had no working action because the provider
/// registered no [`PluginActionExecutor`], so selecting one in `crikey run`
/// reached no plugin at all.
///
/// Routing goes through the same [`PluginActionRouter`] the composition root
/// installs, by item id, exactly as `SearchService` routes a row it did not
/// produce itself. Removing the driver's `action_executor` registration makes
/// the submit fail with "no action runtime owns plugin".
#[test]
fn a_legacy_items_default_action_executes_in_its_owning_plugin() {
    require_legacy_interpreter();

    let well_behaved = PluginId("legacy.well-behaved".to_owned());
    let cache_root = std::env::temp_dir().join("crikey-legacy-action-test-cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = LegacyProvider::load(
        &mut pipeline,
        &[test_plugins_root()],
        cache_root,
        LegacyDirectories::default(),
        LegacyDeadlines::default(),
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&well_behaved),
        "the well-behaved legacy plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let published: Arc<Mutex<Option<ViewModel>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&published);
    let driver = LegacyDriver::spawn(provider, pipeline, move |frame| {
        *sink.lock().expect("the publish sink is not poisoned") = Some(frame.clone());
    });

    let mut router = PluginActionRouter::default();
    router
        .register(driver.plugins(), driver.action_executor())
        .expect("the legacy driver owns its plugin ids exactly once");

    // A row must be on offer before it can be launched: the endpoint validates
    // against the provider's own snapshot, never against a caller's echo.
    let generation = Generation::from_raw(1);
    driver.submit(generation, "report", 17, Vec::new(), false, 0);
    let mut row = None;
    for _ in 0..2_000 {
        if let Some(frame) = published
            .lock()
            .expect("the publish sink is not poisoned")
            .clone()
        {
            if let Some(candidate) = frame
                .rows
                .iter()
                .find(|row| row.plugin_name == well_behaved.0)
                .cloned()
            {
                row = Some(candidate);
                break;
            }
        }
        sleep(Duration::from_millis(5));
    }
    let row = row.expect("the legacy plugin must offer a suggestion to launch");
    let action = row
        .default_action
        .clone()
        .expect("a legacy row must present a default action or Enter does nothing");
    assert_eq!(action.execution_policy, ExecutionPolicy::Plugin);

    let request_id = router
        .submit_by_item_id(&row.item, &action.action_id, None)
        .expect("the exact owner admits its own row's default action");
    assert_eq!(request_id.plugin, well_behaved);

    let mut completion = None;
    for _ in 0..2_000 {
        if let Some(next) = router.poll().pop() {
            completion = Some(next);
            break;
        }
        sleep(Duration::from_millis(5));
    }
    let completion = completion.expect("the admitted action must produce a terminal completion");

    assert_eq!(completion.request_id, request_id);
    assert_eq!(completion.plugin, well_behaved);
    assert_eq!(completion.item_id, row.item);
    assert!(
        completion.outcome.is_ok(),
        "the fixture's `on_execute` succeeds; got {:?}",
        completion.outcome,
    );

    drop(driver);
}

/// An action is a §13.5 unit of work like any other, and a legacy plugin's
/// declared budget must bind it. The refusal has to be attributable, or an
/// operator sees a launch that silently did nothing.
#[test]
fn a_legacy_action_refused_by_its_budget_is_reported_as_an_action_refusal() {
    require_legacy_interpreter();

    let well_behaved = PluginId("legacy.well-behaved".to_owned());
    let cache_root = std::env::temp_dir().join("crikey-legacy-action-budget-test-cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = LegacyProvider::load(
        &mut pipeline,
        &[test_plugins_root()],
        cache_root,
        LegacyDirectories::default(),
        LegacyDeadlines::default(),
        &DisabledPlugins::default(),
    );
    assert!(provider.plugins().contains(&well_behaved));
    let budget = provider
        .plugin_budget(&well_behaved)
        .expect("a loaded legacy plugin has a budget")
        .clone();

    let published: Arc<Mutex<Option<ViewModel>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&published);
    let driver = LegacyDriver::spawn(provider, pipeline, move |frame| {
        *sink.lock().expect("the publish sink is not poisoned") = Some(frame.clone());
    });
    let mut router = PluginActionRouter::default();
    router
        .register(driver.plugins(), driver.action_executor())
        .expect("the legacy driver owns its plugin ids exactly once");

    driver.submit(Generation::from_raw(1), "report", 17, Vec::new(), false, 0);
    let mut row = None;
    for _ in 0..2_000 {
        if let Some(frame) = published
            .lock()
            .expect("the publish sink is not poisoned")
            .clone()
        {
            if let Some(candidate) = frame
                .rows
                .iter()
                .find(|row| row.plugin_name == well_behaved.0)
                .cloned()
            {
                row = Some(candidate);
                break;
            }
        }
        sleep(Duration::from_millis(5));
    }
    let row = row.expect("the legacy plugin must offer a suggestion to launch");
    let action = row
        .default_action
        .clone()
        .expect("a legacy row has a default action");

    // Occupy the single declared action slot the way a still-running action
    // would, then attempt a second launch.
    let held = budget
        .try_acquire_owned(BudgetKind::Action)
        .expect("the declared action slot admits the first unit");
    let error = router
        .submit_by_item_id(&row.item, &action.action_id, None)
        .expect_err("a second action must be refused while the only slot is held");
    assert!(
        error.to_string().contains("action budget is full"),
        "the refusal must say why: {error}",
    );
    assert_eq!(
        budget.refusals(BudgetKind::Action),
        1,
        "the refusal is counted on the shared handle the pipeline also reads",
    );
    drop(held);

    drop(driver);
}

/// The presentation gap: a legacy plugin could name an icon and register
/// alternate actions, and neither reached the renderer — the provider
/// installed no icon resolver at all, so `icon_reference` travelled with
/// nothing behind it, and the host's own action was the only one a row had.
///
/// Proven end to end, on the real `rich-presentation` fixture: the child
/// resolves the plugin's handle to a package-relative name, the provider's
/// [`PluginIconResolver`] reads and decodes the committed PNG, and the row
/// carries both the pixels and the plugin's two alternates behind the host's
/// default action. Deleting `install_icon_resolver` leaves `row.icon` empty
/// while `row.icon_reference` still says there should be one.
#[test]
fn a_legacy_items_loaded_icon_and_actions_reach_the_presented_row() {
    require_legacy_interpreter();

    let rich = PluginId("legacy.rich-presentation".to_owned());
    let cache_root = std::env::temp_dir().join("crikey-legacy-presentation-test-cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = LegacyProvider::load(
        &mut pipeline,
        &[test_plugins_root()],
        cache_root,
        LegacyDirectories::default(),
        LegacyDeadlines::default(),
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&rich),
        "the rich-presentation legacy plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let frame = provider
        .drive_query(&mut pipeline, "rich", 17)
        .expect("the admitted current legacy batch produces a frame");
    let row = frame
        .rows
        .iter()
        .find(|row| row.plugin_name == rich.0 && row.label == "Rich Presentation Entry")
        .unwrap_or_else(|| {
            panic!(
                "the fixture's icon-bearing row must be presented; got {:?}",
                frame
                    .rows
                    .iter()
                    .map(|row| (row.plugin_name.as_str(), row.label.as_str()))
                    .collect::<Vec<_>>(),
            )
        });

    assert_eq!(
        row.icon_reference.as_deref(),
        Some("icons/badge.png"),
        "the plugin's icon handle must reach the row as the name the host resolves",
    );
    // Pixels, not merely a reference: a row that names an icon nothing decoded
    // renders blank, which is indistinguishable from a plugin that shipped no
    // icon at all.
    let icon = row
        .icon
        .as_ref()
        .expect("the committed package icon must be read and decoded for the row");
    assert!(
        icon.width() > 0 && icon.height() > 0 && !icon.rgba().is_empty(),
        "the decoded icon must carry real pixels, got {}x{} with {} bytes",
        icon.width(),
        icon.height(),
        icon.rgba().len(),
    );

    // Enter still means "no secondary action chosen"; the plugin's own
    // registrations are the alternates behind it, in the order it registered
    // them. Promoting one of them to the default would change what pressing
    // Enter on a legacy row does.
    let default_action = row
        .default_action
        .as_ref()
        .expect("a legacy row keeps the host's default action");
    assert_eq!(default_action.action_id.0, "legacy.execute");
    assert_eq!(default_action.execution_policy, ExecutionPolicy::Plugin);
    assert_eq!(
        row.alternate_actions
            .iter()
            .map(|action| action.action_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["copy", "reveal"],
        "the alternates a legacy plugin registered must reach the row it published",
    );

    provider.shutdown(180);
}
