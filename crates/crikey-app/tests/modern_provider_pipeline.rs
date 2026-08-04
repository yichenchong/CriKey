//! A loaded modern Python plugin's suggestions must traverse the app's
//! [`QueryPipeline`] off the UI thread, with every failure contained (contract
//! §8; spec 15.6, 15.7, 24.1; acceptance 31.7, 31.9, 31.10).
//!
//! This is the modern sibling of `legacy_provider_pipeline.rs`. It proves the
//! `modern_provider` is wired into the live query path — not merely reachable
//! from the `crikey dev` commands — by spawning a real child CPython
//! interpreter, running the fixture plugin's `suggest` in that separate
//! process, and showing the item it emits cross the pipeline's bounded intake
//! and come back on the presented frame under the current generation.
//!
//! # Three proofs, one live path
//!
//! * [`modern_suggestions_cross_pipeline_intake_under_current_generation`]
//!   drives the provider directly, keeping the pipeline in hand so its bounded
//!   intake can be inspected: the modern batch is admitted, merged, and
//!   presented under the current generation. A superseded (older-generation)
//!   answer is refused at the intake boundary because `drive_query` only ever
//!   delivers the current generation (acceptance 31.7).
//! * [`modern_worker_crash_is_contained_and_a_sibling_keeps_serving`] loads two
//!   plugins where one crashes its interpreter mid-callback (`os._exit`). The
//!   crash degrades that plugin to a recorded diagnostic and does NOT abort the
//!   provider: a healthy sibling still serves, and a second query still answers
//!   (acceptance 31.9, 31.10).
//! * [`modern_supervisor_publishes_off_the_ui_thread`] drives the same plugin
//!   through the live app path — the [`ModernDriver`] supervisor — and shows the
//!   merged frame arriving asynchronously, published under the generation it was
//!   submitted with rather than a newer one.
//!
//! # A missing interpreter is a failure, not a skip
//!
//! Following the M3 standing rule, a host with no supported CPython fails loudly
//! rather than skipping: the interpreter-search error is quoted so the failure
//! is diagnosable rather than a silent green run.
//!
//! # Isolation and timing
//!
//! Every `suggest` callback runs in a separate operating-system process, never
//! in the test process. The only wall-clock bounds are the worker's startup and
//! per-call budgets the provider passes in, and the bounded poll below that
//! waits for the out-of-process answer — never a `sleep` used as a
//! synchronisation primitive.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use crikey_app::{
    CatalogBuildResult, DisabledPlugins, ModernDriver, ModernProvider, PipelineConfig, PluginActionRouter,
    QueryPipeline,
};
use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, Generation, HitPolicy, Item, ItemId,
    PluginId,
};
use crikey_plugin_supervisor::BudgetKind;
use crikey_python_host::{discover_interpreter, RequiresPython, RuntimeProfile};
use crikey_ui::ViewModel;

/// A private directory removed when the test that made it ends. Mirrors M3's
/// `Scratch`: every fixture (plugin trees, offline index, env cache) is built
/// here at test time, never committed.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-modern-provider-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak fixtures into this one.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.join(name);
        fs::create_dir_all(&path).expect("scratch subdirectory is creatable");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The host plugin id a modern package answers as, mirroring the legacy
/// provider's `legacy.<id>` namespacing discipline (spec 10.2).
fn modern_plugin(id: &str) -> PluginId {
    PluginId(format!("modern.{id}"))
}

/// Fails loudly, never skips, when this host has no supported CPython: the
/// interpreter-search error is quoted so the failure is diagnosable.
fn require_modern_interpreter() {
    if let Err(error) = discover_interpreter(&RuntimeProfile::Bundled, &RequiresPython(">=3.12".to_owned())) {
        panic!("the modern python suite requires a supported CPython on this host: {error}");
    }
}

/// Writes a discoverable modern plugin under `root`: a `<root>/<id>/crikey.toml`
/// manifest declaring the `python` runtime plus the plugin's importable module.
/// The plugin's own directory is its import-path source (contract §3), so
/// `<module>` is importable by the `<module>:<class>` entrypoint.
fn write_modern_plugin(root: &Path, id: &str, module: &str, class_name: &str, body: &str) {
    let dir = root.join(id);
    fs::create_dir_all(&dir).expect("plugin directory is creatable");

    let manifest = format!(
        "manifest-version = 1\n\
         \n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"{id}\"\n\
         version = \"1.0.0\"\n\
         runtime = \"python\"\n\
         entrypoint = \"{module}:{class_name}\"\n\
         \n\
         [python]\n\
         requires-python = \">=3.12\"\n"
    );
    fs::write(dir.join("crikey.toml"), manifest).expect("manifest is writable");
    fs::write(dir.join(format!("{module}.py")), body).expect("plugin module is writable");
}

/// A well-behaved plugin: one deterministic suggestion echoing the query.
const HEALTHY_SOURCE: &str = "\
from crikey_sdk.plugin import Item, Plugin


class Healthy(Plugin):
    def suggest(self, query, context):
        context.emit(
            Item(stable_id=\"healthy-1\", label=\"report \" + query.text, target=\"report\")
        )
";

/// Builds one catalog item through the provider's out-of-band catalog worker.
const CATALOG_SOURCE: &str = "\
from crikey_sdk.plugin import Item, Plugin


class Catalog(Plugin):
    def build_catalog(self):
        return [Item(stable_id=\"catalog-1\", label=\"Catalog item\", target=\"target\")]
";

/// A deliberately slow plugin action. The callback sleeps in its child
/// interpreter so the endpoint's submission path must remain visibly quick.
const ACTION_SOURCE: &str = "\
import time

from crikey_sdk.plugin import Action, Item, Plugin


class ActionPlugin(Plugin):
    def suggest(self, query, context):
        context.emit(
            Item(
                stable_id=\"action-1\",
                label=\"Action\",
                target=\"target\",
                actions=[Action(action_id=\"open\", label=\"Open\", execution_policy=\"plugin\")],
            )
        )

    def execute(self, item, action_id, argument):
        time.sleep(0.2)
";

/// A plugin whose callback kills its interpreter outright (contract §31.10):
/// `os._exit` bypasses cleanup, so the child dies mid-`suggest`.
const CRASHY_SOURCE: &str = "\
import os

from crikey_sdk.plugin import Plugin


class Crashy(Plugin):
    def suggest(self, query, context):
        os._exit(1)
";

/// A plugin that announces an in-flight callback and then cooperatively waits
/// for cancellation. A newer query must interrupt it rather than wait for the
/// full modern call budget.
const CANCELLABLE_SOURCE: &str = "\
import os
import time

from crikey_sdk.plugin import Item, Plugin


class Cancellable(Plugin):
    def suggest(self, query, context):
        with open(os.path.join(os.path.dirname(__file__), \"running\"), \"w\") as marker:
            marker.write(\"1\")
        deadline = time.monotonic() + 3.0
        while time.monotonic() < deadline:
            if context.cancelled:
                return
            time.sleep(0.01)
        context.emit(Item(stable_id=\"cancelled-1\", label=\"late\", target=\"late\"))
";

/// A plugin that ignores cancellation and sleeps long enough to distinguish
/// the manifest hard deadline from the provider's transport upper bound.
const TIMEOUT_SOURCE: &str = "\
import time

from crikey_sdk.plugin import Item, Plugin


class Timeout(Plugin):
    def suggest(self, query, context):
        time.sleep(3.0)
        context.emit(Item(stable_id=\"timeout-1\", label=\"late\", target=\"late\"))
";

/// True when the provider recorded an attributable crash diagnostic for
/// `plugin`, whether as a runtime dispatch failure or a degraded unavailable
/// entry (contract §8: a crash becomes a recorded diagnostic, never a panic).
fn crash_is_recorded(provider: &ModernProvider, plugin: &PluginId) -> bool {
    let dispatched = provider
        .dispatch_failures()
        .iter()
        .any(|(id, reason)| id == plugin && !reason.is_empty());
    let degraded = provider
        .unavailable()
        .iter()
        .any(|entry| entry.plugin.as_ref() == Some(plugin) && !entry.reason.is_empty());
    dispatched || degraded
}

#[test]
fn modern_suggestions_cross_pipeline_intake_under_current_generation() {
    require_modern_interpreter();

    let scratch = Scratch::new("intake");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "healthy",
        "healthy_plugin",
        "Healthy",
        HEALTHY_SOURCE,
    );

    let healthy = modern_plugin("healthy");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );

    // Discovery honours skip-don't-fail: the healthy fixture must have loaded and
    // registered with the pipeline (acceptance 31.9).
    assert!(
        provider.plugins().contains(&healthy),
        "the healthy modern plugin must load and register; unavailable: {:?}",
        provider.unavailable(),
    );

    // A query typed into the app dispatches to the modern plugin, whose child
    // process answers `suggest`, and the item crosses the pipeline.
    let frame = provider
        .drive_query(&mut pipeline, "report", 17)
        .expect("the admitted current modern batch produces a frame");

    // The frame belongs to the query's generation, and it is the pipeline's
    // currently visible one: a stale answer would have been refused at intake
    // rather than shown here (acceptance 31.7).
    assert_eq!(
        pipeline.visible_generation(),
        Some(frame.generation),
        "the presented frame is the current generation",
    );

    // The modern item genuinely traversed the bounded intake: admitted and
    // merged, exactly as the built-in application provider's batch is.
    assert!(
        pipeline.intake_diagnostics().admitted() >= 1,
        "the modern batch must be admitted to intake",
    );
    assert!(
        pipeline.intake_diagnostics().merged() >= 1,
        "the modern batch must be merged out of intake",
    );

    // And it is present on the frame, owned by the modern plugin.
    assert!(
        frame.rows.iter().any(|row| row.plugin_name == healthy.0),
        "the presented frame must carry the modern plugin's suggestion, found: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );

    provider.shutdown(180);
}

#[test]
fn modern_worker_crash_is_contained_and_a_sibling_keeps_serving() {
    require_modern_interpreter();

    let scratch = Scratch::new("crash");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "healthy",
        "healthy_plugin",
        "Healthy",
        HEALTHY_SOURCE,
    );
    write_modern_plugin(&plugins_root, "crashy", "crashy_plugin", "Crashy", CRASHY_SOURCE);

    let healthy = modern_plugin("healthy");
    let crashy = modern_plugin("crashy");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );

    // Both plugins load cleanly — the crash is a *runtime* fault in `suggest`,
    // not a load fault — so both are registered before any query.
    assert!(
        provider.plugins().contains(&healthy),
        "the healthy plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );
    assert!(
        provider.plugins().contains(&crashy),
        "the crashing plugin must also load before it faults; unavailable: {:?}",
        provider.unavailable(),
    );

    // Driving the query runs both children. The crash of one MUST NOT abort the
    // provider: a frame is still produced for the healthy sibling.
    let frame = provider
        .drive_query(&mut pipeline, "report", 17)
        .expect("a crashing plugin must not stop the healthy sibling from producing a frame");

    assert!(
        frame.rows.iter().any(|row| row.plugin_name == healthy.0),
        "the healthy sibling must still serve despite the crash, found: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );
    assert!(
        !frame.rows.iter().any(|row| row.plugin_name == crashy.0),
        "the crashed plugin must contribute no rows",
    );

    // The crash is contained as an attributable diagnostic, never a panic
    // (contract §8, acceptance 31.10).
    assert!(
        crash_is_recorded(&provider, &crashy),
        "the interpreter crash must be recorded against the plugin; \
         dispatch_failures: {:?}, unavailable: {:?}",
        provider.dispatch_failures(),
        provider.unavailable(),
    );

    // A second query proves the provider (and the process) stayed alive: the
    // healthy sibling still answers after the crash.
    let again = provider
        .drive_query(&mut pipeline, "report again", 42)
        .expect("the provider keeps serving after a contained crash");
    assert!(
        again.rows.iter().any(|row| row.plugin_name == healthy.0),
        "the healthy sibling keeps serving on the next query, found: {:?}",
        again
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );

    provider.shutdown(180);
}

#[test]
fn modern_supervisor_publishes_off_the_ui_thread() {
    require_modern_interpreter();

    let scratch = Scratch::new("supervisor");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "healthy",
        "healthy_plugin",
        "Healthy",
        HEALTHY_SOURCE,
    );

    let healthy = modern_plugin("healthy");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&healthy),
        "the healthy modern plugin must load and register; unavailable: {:?}",
        provider.unavailable(),
    );

    // The live app path drives the plugin off the UI thread. The supervisor
    // publishes each merged frame through this callback, exactly as the launcher
    // forwards it to the renderer.
    let published: Arc<Mutex<Option<ViewModel>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&published);
    let driver = ModernDriver::spawn(provider, pipeline, move |frame| {
        *sink.lock().expect("the publish sink is not poisoned") = Some(frame.clone());
    });
    assert!(
        driver.has_plugins(),
        "the supervisor must serve the loaded modern plugin",
    );

    // Submitting returns immediately: the UI thread never blocks on the child
    // interpreter (spec 6.5, acceptance 31.1). The merged frame is tagged with
    // this generation and must never surface under a newer one.
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
    let frame = frame.expect("the supervisor must publish a modern frame off the UI thread");

    assert_eq!(
        frame.generation, generation,
        "the async frame is published under the generation it was submitted with, never a newer one",
    );
    assert!(
        frame.rows.iter().any(|row| row.plugin_name == healthy.0),
        "the async frame must carry the modern plugin's suggestion, found: {:?}",
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

/// Plugin A: a distinct item so it cannot be confused with plugin B's answer.
const ALPHA_SOURCE: &str = "\
from crikey_sdk.plugin import Item, Plugin


class Provider(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id=\"a-1\", label=\"alpha-item\", target=\"a\"))
";

/// Plugin B: the SAME entrypoint string (`provider:Provider`) and an identical
/// empty environment as plugin A, but a different source dir and a different
/// item — so a shared worker would serve the wrong plugin's results.
const BETA_SOURCE: &str = "\
from crikey_sdk.plugin import Item, Plugin


class Provider(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id=\"b-1\", label=\"beta-item\", target=\"b\"))
";

/// A plugin declaring a §19.4 `minimum-query-length`, to prove the pipeline
/// schedules it under the manifest-derived policy rather than the flat default.
const GATED_SOURCE: &str = "\
from crikey_sdk.plugin import Item, Plugin


class Gated(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id=\"gated-1\", label=\"gated hit\", target=\"g\"))
";

#[test]
fn distinct_source_dirs_do_not_share_a_worker() {
    require_modern_interpreter();

    let scratch = Scratch::new("identity");
    let plugins_root = scratch.subdir("plugins");
    // Same entrypoint string and identical (empty) environments; only the source
    // directory differs. A `(env, entrypoint)`-only key collapses these onto one
    // worker (pinned decision 1).
    write_modern_plugin(&plugins_root, "alpha", "provider", "Provider", ALPHA_SOURCE);
    write_modern_plugin(&plugins_root, "beta", "provider", "Provider", BETA_SOURCE);

    let alpha = modern_plugin("alpha");
    let beta = modern_plugin("beta");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&alpha) && provider.plugins().contains(&beta),
        "both plugins must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let frame = provider
        .drive_query(&mut pipeline, "q", 17)
        .expect("both plugins produce a frame");

    let alpha_row = frame.rows.iter().find(|row| row.plugin_name == alpha.0);
    let beta_row = frame.rows.iter().find(|row| row.plugin_name == beta.0);
    assert!(
        alpha_row.is_some_and(|row| row.label == "alpha-item"),
        "plugin alpha must answer with its own item; rows: {:?}",
        frame
            .rows
            .iter()
            .map(|r| (r.plugin_name.clone(), r.label.clone()))
            .collect::<Vec<_>>(),
    );
    // Kills the shared-worker mutation: with a `(env, entrypoint)`-only key,
    // plugin beta is served by alpha's worker and its row reads "alpha-item".
    assert!(
        beta_row.is_some_and(|row| row.label == "beta-item"),
        "plugin beta must answer with ITS OWN worker's item, not a shared \
         worker's; rows: {:?}",
        frame
            .rows
            .iter()
            .map(|r| (r.plugin_name.clone(), r.label.clone()))
            .collect::<Vec<_>>(),
    );

    provider.shutdown(180);
}

#[test]
fn a_crashed_worker_records_one_failure_and_stays_unavailable() {
    require_modern_interpreter();

    let scratch = Scratch::new("bounded");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "healthy",
        "healthy_plugin",
        "Healthy",
        HEALTHY_SOURCE,
    );
    write_modern_plugin(&plugins_root, "crashy", "crashy_plugin", "Crashy", CRASHY_SOURCE);

    let healthy = modern_plugin("healthy");
    let crashy = modern_plugin("crashy");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&healthy) && provider.plugins().contains(&crashy),
        "both plugins load before the crash; unavailable: {:?}",
        provider.unavailable(),
    );

    // Several queries after the crash: the healthy sibling keeps serving on each.
    for iteration in 0..5 {
        let now = 100 * (iteration as u64 + 1);
        let frame = provider.drive_query(&mut pipeline, &format!("report {iteration}"), now);
        assert!(
            frame.is_some_and(|frame| frame.rows.iter().any(|row| row.plugin_name == healthy.0)),
            "the healthy sibling keeps serving on query {iteration} despite the crash",
        );
    }

    // The crash is recorded exactly ONCE, not once per keystroke: kills the
    // "push an unbounded failure entry every query" mutation.
    let crashy_failures = provider
        .dispatch_failures()
        .iter()
        .filter(|(plugin, _)| plugin == &crashy)
        .count();
    assert_eq!(
        crashy_failures,
        1,
        "a crashed plugin's failure is recorded once, not per keystroke; failures: {:?}",
        provider.dispatch_failures(),
    );
    assert!(
        crash_is_recorded(&provider, &crashy),
        "the crashed plugin stays cleanly recorded as unavailable",
    );

    provider.shutdown(180);
}

#[test]
fn modern_plugin_is_scheduled_under_its_manifest_query_policy() {
    require_modern_interpreter();

    let scratch = Scratch::new("policy");
    let plugins_root = scratch.subdir("plugins");
    let dir = plugins_root.join("gated");
    fs::create_dir_all(&dir).expect("plugin directory is creatable");
    // A manifest declaring `[activation] minimum-query-length = 5` (§19.4). The
    // flat `PluginPolicy::modern()` default is 0, so only a manifest-derived
    // policy gates a short query.
    let manifest = "manifest-version = 1\n\
                    \n\
                    [plugin]\n\
                    id = \"gated\"\n\
                    name = \"gated\"\n\
                    version = \"1.0.0\"\n\
                    runtime = \"python\"\n\
                    entrypoint = \"gated_plugin:Gated\"\n\
                    \n\
                    [python]\n\
                    requires-python = \">=3.12\"\n\
                    \n\
                    [activation]\n\
                    minimum-query-length = 5\n";
    fs::write(dir.join("crikey.toml"), manifest).expect("manifest is writable");
    fs::write(dir.join("gated_plugin.py"), GATED_SOURCE).expect("plugin module is writable");

    let gated = modern_plugin("gated");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&gated),
        "the plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    // A query below the declared minimum must NOT dispatch the plugin. Under the
    // flat default (0) it would, so this assertion kills the "register the flat
    // default instead of the manifest policy" mutation.
    let short = provider.drive_query(&mut pipeline, "hi", 17);
    assert!(
        short.is_none_or(|frame| { !frame.rows.iter().any(|row| row.plugin_name == gated.0) }),
        "a query below the declared minimum-query-length must not dispatch the plugin",
    );

    // A query at or above the minimum dispatches and the plugin serves, proving
    // the no-row above was the policy gate, not a broken plugin.
    let long = provider
        .drive_query(&mut pipeline, "hello there", 5_000)
        .expect("a query above the minimum produces a frame");
    assert!(
        long.rows.iter().any(|row| row.plugin_name == gated.0),
        "a query above the declared minimum must dispatch the plugin; rows: {:?}",
        long.rows
            .iter()
            .map(|r| (r.plugin_name.clone(), r.label.clone()))
            .collect::<Vec<_>>(),
    );

    provider.shutdown(180);
}

#[test]
fn modern_driver_refuses_a_superseded_generation() {
    require_modern_interpreter();

    let scratch = Scratch::new("supersede");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "healthy",
        "healthy_plugin",
        "Healthy",
        HEALTHY_SOURCE,
    );

    let healthy = modern_plugin("healthy");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&healthy),
        "the healthy plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    // Record EVERY published frame's generation, so a superseded frame that
    // slips through is observable rather than silently overwritten.
    let published: Arc<Mutex<Vec<Generation>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let driver = ModernDriver::spawn(provider, pipeline, move |frame| {
        sink.lock()
            .expect("the publish sink is not poisoned")
            .push(frame.generation);
    });
    assert!(
        driver.has_plugins(),
        "the supervisor must serve the loaded plugin"
    );

    // Two generations submitted back to back: the older is superseded by the
    // newer before the child interpreter can answer it.
    let older = Generation::from_raw(1);
    let newer = Generation::from_raw(2);
    driver.submit(older, "report old", 17, Vec::new(), false, 0);
    driver.submit(newer, "report new", 18, Vec::new(), false, 0);

    // Bounded poll for the newer generation's frame; only waits for the
    // out-of-process answer, never a sleep used to order events.
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

    // A delayed caller must also be unable to rewind the live generation after
    // the newer frame is already visible. Without a monotonic intake guard,
    // this submission queues an obsolete frame and publishes generation 1.
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

#[test]
fn modern_driver_cancels_a_superseded_in_flight_callback() {
    require_modern_interpreter();

    let scratch = Scratch::new("cancel-in-flight");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "cancellable",
        "cancellable_plugin",
        "Cancellable",
        CANCELLABLE_SOURCE,
    );

    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root.clone()],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    let plugin = modern_plugin("cancellable");
    assert!(
        provider.plugins().contains(&plugin),
        "the cancellable plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let published: Arc<Mutex<Vec<Generation>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let driver = ModernDriver::spawn(provider, pipeline, move |frame| {
        sink.lock()
            .expect("the publish sink is not poisoned")
            .push(frame.generation);
    });

    let older = Generation::from_raw(1);
    let newer = Generation::from_raw(2);
    driver.submit(older, "old", 17, Vec::new(), false, 0);
    let running = plugins_root.join("cancellable").join("running");
    for _ in 0..2_000 {
        if running.is_file() {
            break;
        }
        sleep(Duration::from_millis(5));
    }
    assert!(
        running.is_file(),
        "the first callback must be in flight before supersession"
    );

    let started = std::time::Instant::now();
    driver.submit(newer, "new", 18, Vec::new(), false, 0);
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
    assert!(
        seen_newer,
        "the newer query must complete after cancelling the old callback (elapsed {:?})",
        started.elapsed(),
    );
    let generations = published
        .lock()
        .expect("the publish sink is not poisoned")
        .clone();
    assert!(
        generations.iter().all(|generation| *generation == newer),
        "the cancelled callback must never publish its old generation; saw {generations:?}",
    );

    drop(driver);
}

#[test]
fn modern_manifest_hard_deadline_limits_suggest_call() {
    require_modern_interpreter();

    let scratch = Scratch::new("manifest-timeout");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "timeout",
        "timeout_plugin",
        "Timeout",
        TIMEOUT_SOURCE,
    );
    let manifest_path = plugins_root.join("timeout").join("crikey.toml");
    let mut manifest = fs::read_to_string(&manifest_path).expect("manifest is readable");
    manifest.push_str("\n[performance]\nsuggest-hard-timeout-ms = 100\n");
    fs::write(&manifest_path, manifest).expect("manifest deadline is writable");

    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    let plugin = modern_plugin("timeout");
    assert!(
        provider.plugins().contains(&plugin),
        "the timeout plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let started = std::time::Instant::now();
    let frame = provider
        .drive_query(&mut pipeline, "timeout", 17)
        .expect("a timed-out plugin still yields a current empty frame");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the manifest hard deadline must stop suggest near 100 ms, not the 120 s transport cap (elapsed {:?})",
        started.elapsed(),
    );
    assert!(
        !frame.rows.iter().any(|row| row.plugin_name == plugin.0),
        "a timed-out plugin must contribute no rows",
    );

    provider.shutdown(0);
}

#[test]
fn modern_catalog_build_uses_and_releases_the_catalog_budget() {
    require_modern_interpreter();

    let scratch = Scratch::new("catalog-budget");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "catalog",
        "catalog_plugin",
        "Catalog",
        CATALOG_SOURCE,
    );

    let catalog_plugin = modern_plugin("catalog");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&catalog_plugin),
        "the catalog plugin must load; unavailable: {:?}",
        provider.unavailable()
    );

    let request_id = provider
        .request_catalog_build(&catalog_plugin, 7, Generation::from_raw(3))
        .expect("the loaded plugin's catalog request is admitted");
    assert!(request_id > 0);

    let mut results = None;
    for _ in 0..2_000 {
        let ready = provider.take_catalog_results();
        if !ready.is_empty() {
            results = Some(ready);
            break;
        }
        sleep(Duration::from_millis(5));
    }
    let result = results
        .expect("the bounded catalog worker must produce an observable result")
        .into_iter()
        .next()
        .expect("one request produces one result");
    match result {
        CatalogBuildResult::Complete(build) => {
            assert_eq!(build.plugin, catalog_plugin);
            assert_eq!(build.instance, 7);
            assert_eq!(build.generation, Generation::from_raw(3));
            assert_eq!(build.items.len(), 1);
            assert_eq!(build.items[0].stable_id.0, "catalog-1");
        }
        other => panic!("catalog request failed unexpectedly: {other:?}"),
    }

    assert_eq!(
        pipeline
            .plugin_budget(&catalog_plugin)
            .expect("the plugin remains registered")
            .in_flight(BudgetKind::Catalog),
        0,
        "catalog completion must release the shared catalog slot"
    );
    provider.shutdown(0);
}

#[test]
fn modern_action_submission_is_nonblocking_and_budgeted() {
    require_modern_interpreter();

    let scratch = Scratch::new("action-budget");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "action",
        "action_plugin",
        "ActionPlugin",
        ACTION_SOURCE,
    );

    let action_plugin = modern_plugin("action");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&action_plugin),
        "the action plugin must load; unavailable: {:?}",
        provider.unavailable()
    );
    provider
        .drive_query(&mut pipeline, "action", 17)
        .expect("the action item must cross the live suggestion path");

    let item = Item {
        stable_id: ItemId("action-1".to_owned()),
        plugin_id: action_plugin.clone(),
        category: Category::PluginDefined("plugin-defined".to_owned()),
        label: "Action".to_owned(),
        description: String::new(),
        target: "target".to_owned(),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: std::collections::BTreeMap::new(),
        actions: vec![Action {
            action_id: ActionId("open".to_owned()),
            label: "Open".to_owned(),
            description: String::new(),
            applicable_categories: Vec::new(),
            icon_reference: None,
            execution_policy: ExecutionPolicy::Plugin,
        }],
    };
    let driver = ModernDriver::spawn(provider, pipeline, |_| {});
    let endpoint = driver.action_executor();
    let mut wrong_owner = item.clone();
    wrong_owner.plugin_id = modern_plugin("other");
    let ownership_error = endpoint
        .submit_plugin_action(&action_plugin, &wrong_owner, &ActionId("open".to_owned()), None)
        .expect_err("an item attributed to another plugin must never be dispatched here");
    assert!(
        ownership_error.to_string().contains("stale ownership"),
        "the provider must reject an item whose owner does not match the routed plugin: {ownership_error}",
    );

    let started = std::time::Instant::now();
    let first = endpoint
        .submit_plugin_action(&action_plugin, &item, &ActionId("open".to_owned()), None)
        .expect("the first action is admitted");
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "action submission must not wait for the slow child callback"
    );

    let refused = endpoint
        .submit_plugin_action(&action_plugin, &item, &ActionId("open".to_owned()), None)
        .expect_err("the shared action budget must refuse concurrent work");
    assert!(
        refused.to_string().contains("action budget"),
        "refusal must identify the action budget: {refused}"
    );

    let mut completions = None;
    for _ in 0..2_000 {
        let ready = endpoint.poll_plugin_actions();
        if !ready.is_empty() {
            completions = Some(ready);
            break;
        }
        sleep(Duration::from_millis(5));
    }
    let completion = completions
        .expect("the slow action must eventually produce a terminal completion")
        .into_iter()
        .find(|completion| completion.request_id == first)
        .expect("the first request's completion must be attributed exactly");
    assert!(completion.outcome.is_ok(), "the plugin action should succeed");
    drop(driver);
    let gone = endpoint
        .submit_plugin_action(&action_plugin, &item, &ActionId("open".to_owned()), None)
        .expect_err("an action submitted after the owning driver stops must be rejected");
    assert!(
        gone.to_string().contains("runtime stopped"),
        "a gone plugin must report dispatch failure instead of silently dropping the action: {gone}",
    );
}

#[test]
fn modern_router_rejects_duplicate_stable_ids_across_plugin_owners() {
    require_modern_interpreter();

    let scratch = Scratch::new("duplicate-action-id");
    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(
        &plugins_root,
        "alpha",
        "alpha_action",
        "ActionPlugin",
        ACTION_SOURCE,
    );
    write_modern_plugin(
        &plugins_root,
        "beta",
        "beta_action",
        "ActionPlugin",
        ACTION_SOURCE,
    );

    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert_eq!(provider.plugins().len(), 2);
    provider
        .drive_query(&mut pipeline, "action", 17)
        .expect("both duplicate-id plugins must publish their current snapshots");

    let driver = ModernDriver::spawn(provider, pipeline, |_| {});
    let mut router = PluginActionRouter::default();
    router
        .register(driver.plugins(), driver.action_executor())
        .expect("the router registers each modern owner exactly once");
    let error = router
        .submit_by_item_id(&ItemId("action-1".to_owned()), &ActionId("open".to_owned()), None)
        .expect_err("a stable id shared by two owners must not be routed arbitrarily");
    assert!(
        error.to_string().contains("ambiguous ownership"),
        "duplicate stable ids must be rejected explicitly: {error}"
    );
    drop(driver);
}
