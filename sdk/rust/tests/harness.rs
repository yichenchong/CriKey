//! Red-first tests for the SDK's in-process harness, layout validator, and
//! benchmark helper (spec 16.7, 16.5, 9.4, 23.3, and 24.3).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

use crikey_core::{Item, Result};
use crikey_native_protocol::{Capabilities, Endpoint};
use crikey_plugin_sdk::{
    bench::{measure, BenchReport},
    harness::{BatchStateKind, TestHarness},
    packaging::validate_layout,
    CatalogSink, ExecuteRequest, ItemBuilder, Plugin, PluginContext, Query, SdkError, ServeConfig,
    SuggestionSink,
};

fn config() -> ServeConfig {
    ServeConfig {
        plugin_id: "harness.test".to_owned(),
        plugin_name: "Harness Test Plugin".to_owned(),
        plugin_version: "2.0.0".to_owned(),
        sdk_version: "sdk-test".to_owned(),
        capabilities: Capabilities {
            streaming_catalog: true,
            streaming_suggestions: true,
            cancellation: true,
            configuration_updates: false,
            events: false,
        },
        endpoint: Some(Endpoint::Stdio),
        session_token: Some("harness-session".to_owned()),
    }
}

fn item(stable_id: impl Into<String>, label: impl Into<String>) -> Item {
    ItemBuilder::new(stable_id, label)
        .target("harness-target")
        .build()
}

struct HarnessPlugin {
    executed: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    fail_start: bool,
    fail_suggest: bool,
    emit_logs: bool,
    many_results: bool,
}

impl HarnessPlugin {
    fn new(executed: Arc<AtomicBool>, stopped: Arc<AtomicBool>) -> Self {
        Self {
            executed,
            stopped,
            fail_start: false,
            fail_suggest: false,
            emit_logs: false,
            many_results: false,
        }
    }

    fn failing_start() -> Self {
        Self {
            executed: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            fail_start: true,
            fail_suggest: false,
            emit_logs: false,
            many_results: false,
        }
    }

    fn with_logs(mut self) -> Self {
        self.emit_logs = true;
        self
    }

    fn failing_suggest(mut self) -> Self {
        self.fail_suggest = true;
        self
    }

    fn with_many_results(mut self) -> Self {
        self.many_results = true;
        self
    }
}

impl Plugin for HarnessPlugin {
    fn start(&mut self, _context: &dyn PluginContext) -> Result<()> {
        if self.fail_start {
            return Err(crikey_core::CoreError::Invalid(
                "fixture startup failure".to_owned(),
            ));
        }
        Ok(())
    }

    fn build_catalog(&mut self, context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        if self.emit_logs {
            context.log(crikey_plugin_sdk::LogLevel::Info, "catalog log");
        }
        sink.emit_batch(vec![
            item("catalog-one", "Catalog One"),
            item("catalog-two", "Catalog Two"),
        ])?;
        sink.finish()
    }

    fn suggest(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        if self.fail_suggest {
            return Err(crikey_core::CoreError::Invalid(
                "fixture suggestion failure".to_owned(),
            ));
        }
        if self.emit_logs {
            context.log(crikey_plugin_sdk::LogLevel::Info, "suggestion log");
        }
        if self.many_results {
            let items = (0..10_001)
                .map(|index| item(format!("many-{index}"), format!("many {index}")))
                .collect();
            sink.emit_batch(items)?;
            return sink.finish();
        }
        if sink.is_cancelled() {
            return sink.finish();
        }
        sink.emit_batch(vec![item("suggest-one", format!("{} one", query.text))])?;
        sink.emit_batch(vec![item("suggest-two", format!("{} two", query.text))])?;
        sink.finish()
    }

    fn execute(&mut self, _request: ExecuteRequest, context: &dyn PluginContext) -> Result<()> {
        if self.emit_logs {
            context.log(crikey_plugin_sdk::LogLevel::Info, "execute log");
        }
        self.executed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&mut self, _context: &dyn PluginContext) -> Result<()> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn test_harness_drives_handshake_catalog_suggest_execute_and_shutdown() {
    let executed = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let mut harness = TestHarness::start(
        HarnessPlugin::new(Arc::clone(&executed), Arc::clone(&stopped)),
        config(),
    )
    .expect("start in-process harness");

    let handshake = harness.handshake();
    assert_eq!(handshake.plugin_id, "harness.test");
    assert_eq!(handshake.plugin_version, "2.0.0");
    assert!(handshake.capabilities.streaming_catalog);
    assert!(handshake.capabilities.streaming_suggestions);
    assert!(handshake.capabilities.cancellation);

    let catalog = harness.catalog().expect("catalog request");
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0].stable_id.0, "catalog-one");
    assert_eq!(catalog[1].stable_id.0, "catalog-two");

    let suggestions = harness.suggest("query").expect("suggest request");
    assert_eq!(suggestions.state, BatchStateKind::Final);
    assert_eq!(suggestions.batches, 3, "two partials plus terminal batch");
    assert_eq!(suggestions.items.len(), 2);
    assert_eq!(suggestions.items[0].label, "query one");
    assert_eq!(suggestions.items[1].label, "query two");

    harness
        .execute("catalog-one", Some("open"), Some("argument"))
        .expect("execute request");
    assert!(executed.load(Ordering::SeqCst));

    harness.shutdown().expect("plugin shutdown");
    assert!(stopped.load(Ordering::SeqCst));
}

#[test]
fn test_harness_matches_host_log_and_failure_result_handling() {
    let executed = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let mut harness = TestHarness::start(
        HarnessPlugin::new(Arc::clone(&executed), Arc::clone(&stopped)).with_logs(),
        config(),
    )
    .expect("start logging plugin");

    let catalog = harness.catalog().expect("catalog with log");
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0].stable_id.0, "catalog-one");
    assert_eq!(catalog[1].stable_id.0, "catalog-two");
    let suggestions = harness.suggest("logged").expect("suggestion with log");
    assert_eq!(suggestions.state, BatchStateKind::Final);
    harness
        .execute("catalog-one", None, None)
        .expect("execute with log");
    assert!(executed.load(Ordering::SeqCst));
    harness.shutdown().expect("shutdown logging plugin");

    let mut failing = TestHarness::start(
        HarnessPlugin::new(Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)))
            .failing_suggest(),
        config(),
    )
    .expect("start failing suggestion plugin");
    let result = failing.suggest("failure").expect("failed suggestion frame");
    assert_eq!(result.state, BatchStateKind::Failed);
    failing.shutdown().expect("shutdown failing plugin");
}

#[test]
fn test_harness_applies_the_host_result_limit() {
    let mut harness = TestHarness::start(
        HarnessPlugin::new(Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)))
            .with_many_results(),
        config(),
    )
    .expect("start oversized plugin");

    let result = harness.suggest("many").expect("bounded suggestion");
    assert_eq!(result.state, BatchStateKind::Final);
    assert_eq!(result.items.len(), 10_000);
    harness.shutdown().expect("shutdown oversized plugin");
}

#[test]
fn test_harness_reports_a_plugin_start_failure_before_returning() {
    let error = match TestHarness::start(HarnessPlugin::failing_start(), config()) {
        Ok(_) => panic!("a plugin that fails during start must not produce a harness"),
        Err(error) => error,
    };
    match error {
        SdkError::Protocol(detail) => assert!(
            detail.contains("fixture startup failure"),
            "startup error should preserve the plugin detail: {detail}"
        ),
        other => panic!("startup failure returned the wrong error: {other:?}"),
    }
}

#[test]
fn test_harness_cancel_latch_produces_cancelled_then_plain_suggest_clears_it() {
    let executed = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let mut harness = TestHarness::start(HarnessPlugin::new(executed, Arc::clone(&stopped)), config())
        .expect("start in-process harness");

    harness.cancel();
    let cancelled = harness
        .suggest_with_cancel_latched("cancelled")
        .expect("latched cancellation request");
    assert_eq!(cancelled.state, BatchStateKind::Cancelled);

    let final_result = harness.suggest("normal").expect("ordinary request");
    assert_eq!(final_result.state, BatchStateKind::Final);
    assert_eq!(final_result.items.len(), 2);

    harness.shutdown().expect("plugin shutdown");
    assert!(stopped.load(Ordering::SeqCst));
}

#[test]
fn bench_measure_reports_requested_iterations_and_all_driven_items() {
    let executed = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let mut harness = TestHarness::start(HarnessPlugin::new(executed, Arc::clone(&stopped)), config())
        .expect("start in-process harness");

    let report: BenchReport = measure(&mut harness, &["benchmark"], 4).expect("measure");
    assert_eq!(report.iterations, 4);
    assert_eq!(report.items, 8, "four requests each returned two items");
    assert!(report.p50_us <= report.p95_us, "percentiles must be monotonic");

    harness.shutdown().expect("plugin shutdown");
    assert!(stopped.load(Ordering::SeqCst));
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("crikey-sdk-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("create SDK layout scratch directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn native_manifest(os: &str, arch: &str, entrypoint: &str, spelling: &str) -> String {
    let entrypoint_line = match spelling {
        "inline" => format!("entrypoint = {{ \"{os}-{arch}\" = \"{entrypoint}\" }}"),
        "dotted" => format!("entrypoint.{os}-{arch} = \"{entrypoint}\""),
        "scalar" => format!("entrypoint = \"{entrypoint}\""),
        other => panic!("unknown entrypoint spelling {other}"),
    };
    format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"layout.test\"\n\
         name = \"Layout Test\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         {entrypoint_line}\n"
    )
}

#[test]
fn packaging_validate_layout_accepts_all_documented_entrypoint_spellings() {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let relative = if cfg!(windows) {
        "bin/plugin.exe"
    } else {
        "bin/plugin"
    };
    for spelling in ["inline", "dotted", "scalar"] {
        let scratch = Scratch::new(&format!("layout-valid-{spelling}"));
        let entrypoint = scratch.path().join(relative);
        fs::create_dir_all(entrypoint.parent().expect("entrypoint parent"))
            .expect("create entrypoint directory");
        fs::write(&entrypoint, b"native plugin binary").expect("write entrypoint fixture");
        let manifest = scratch.path().join("crikey.toml");
        fs::write(&manifest, native_manifest(os, arch, relative, spelling)).expect("write manifest fixture");

        let layout = validate_layout(scratch.path(), os, arch).expect("valid native layout");
        assert_eq!(layout.manifest, manifest, "{spelling} manifest path");
        assert_eq!(layout.entrypoint, entrypoint, "{spelling} entrypoint path");
    }
}

#[test]
fn packaging_validate_layout_rejects_a_missing_platform_entrypoint_as_config_error() {
    let scratch = Scratch::new("layout-missing");
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let relative = if cfg!(windows) {
        "bin/missing.exe"
    } else {
        "bin/missing"
    };
    let manifest = scratch.path().join("crikey.toml");
    fs::write(&manifest, native_manifest(os, arch, relative, "inline")).expect("write manifest fixture");

    let error = match validate_layout(scratch.path(), os, arch) {
        Ok(_) => panic!("missing binary must reject"),
        Err(error) => error,
    };
    match error {
        SdkError::Config(detail) => assert!(!detail.is_empty(), "missing-binary diagnostic"),
        other => panic!("missing entrypoint returned the wrong error: {other:?}"),
    }
}

/// Number of items the plugin below hands to a single `emit_batch` call. It
/// is comfortably past what one decodable frame can carry, which is the
/// point: the SDK must split rather than emit a frame the receiver refuses.
const HUGE_CATALOG_ITEMS: usize = 40_000;

/// A well-behaved plugin that simply has a large catalog and emits it in one
/// call, exactly as the `CatalogSink` contract invites.
#[derive(Debug, Default)]
struct HugeCatalogPlugin;

impl Plugin for HugeCatalogPlugin {
    fn start(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    fn build_catalog(&mut self, _context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        let items = (0..HUGE_CATALOG_ITEMS)
            .map(|index| {
                ItemBuilder::new(format!("huge.{index:06}"), format!("Huge Item {index:06}"))
                    .target(format!("/synthetic/app-{index:06}"))
                    .description("a catalog entry of unremarkable size")
                    .search_term(format!("huge-{index:06}"))
                    .build()
            })
            .collect();
        sink.emit_batch(items)?;
        sink.finish()
    }

    fn suggest(
        &mut self,
        _query: Query,
        _context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        sink.finish()
    }

    fn execute(&mut self, _request: ExecuteRequest, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
}

/// A catalog larger than one decodable frame arrives whole.
///
/// The plugin does nothing wrong: it fills a batch and emits it. Before the
/// SDK split on `message::max_decodable_items`, that produced one frame that
/// passed the wire cap and then failed to decode, and the host answered by
/// disconnecting the plugin. Every item must now arrive, across as many
/// frames as the decode budget requires.
#[test]
fn a_catalog_larger_than_one_decodable_frame_arrives_whole() {
    let mut harness = TestHarness::start(HugeCatalogPlugin, config()).expect("harness starts");
    let items = harness
        .catalog()
        .expect("a plugin that emits its whole catalog in one call must not be punished for it");
    assert_eq!(items.len(), HUGE_CATALOG_ITEMS, "the split must lose nothing");
    assert_eq!(items[0].stable_id.0, "huge.000000");
    assert_eq!(
        items[HUGE_CATALOG_ITEMS - 1].stable_id.0,
        format!("huge.{:06}", HUGE_CATALOG_ITEMS - 1),
        "order must be preserved across the split"
    );
    harness.shutdown().expect("harness shuts down");
}
