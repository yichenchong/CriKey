//! Safe mode must actually stop third-party plugins loading (spec 24.2;
//! roadmap M6).
//!
//! `startup_recovery.rs` pins the journal: when it decides an install has
//! failed repeatedly, [`StartupMode::SafeMode`] comes back. That decision buys
//! nothing on its own. Spec 24.2 says safe mode runs *with third-party plugins
//! disabled*, so the guarantee under test here is the seam where the mode meets
//! the loader: the same native package on disk must load under
//! [`StartupMode::Normal`] and must not exist at all under
//! [`StartupMode::SafeMode`].
//!
//! These tests therefore drive the real [`NativeProvider`] against the real
//! out-of-tree conformance executable, exactly like
//! `native_provider_pipeline.rs`. An in-process fake would prove only that a
//! boolean was read; only a real load proves no third-party process is
//! admitted. Both directions are asserted in one test so a provider that
//! always loads nothing cannot pass.
//!
//! # Determinism
//!
//! Each test writes its package into a uniquely named scratch directory that
//! is removed when the test ends, builds the conformance workspace behind a
//! process-wide `LazyLock`, and never sleeps to order two actions.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::LazyLock;

use crikey_app::{
    admitted_plugin_roots, BatchState, NativeProvider, PipelineConfig, QueryPipeline, ResultBatch,
    StartupMode, SAFE_MODE_AFTER_FAILURES,
};
use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_input_scheduler::{CompletionOutcome, DebouncePolicy, Millis, PluginPolicy, SchedulingProfile};

/// Virtual timestamp used by the hand-driven pipeline test below.
const NOW: Millis = 17;

/// A private directory removed when the test that made it ends. Every package
/// manifest and mode witness is written at test time, never committed.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-safe-mode-{label}-{}-{}",
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

/// The host plugin id a native package answers as (contract §6).
fn native_plugin(id: &str) -> PluginId {
    PluginId(format!("native.{id}"))
}

/// Finds the repository root by walking to the directory containing
/// `compatibility/`, as required by conformance contract §8.
fn workspace_root() -> PathBuf {
    let mut directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if directory.join("compatibility").is_dir() {
            return directory;
        }
        assert!(
            directory.pop(),
            "could not find the workspace root containing compatibility/ from {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
}

/// The healthy out-of-tree conformance plugin, built once per test process.
/// Cargo's own lock serializes concurrent integration-test processes.
static CONFORMANCE_BINARY: LazyLock<PathBuf> = LazyLock::new(|| {
    let root = workspace_root();
    let manifest = root.join("compatibility/native-conformance/Cargo.toml");
    let target = root.join("target/native-conformance");
    let output = Command::new("cargo")
        .arg("build")
        .arg("--bins")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "failed to invoke cargo for the native conformance workspace {}: {error}",
                manifest.display()
            )
        });
    assert!(
        output.status.success(),
        "native conformance workspace failed to build ({}):\nstdout:\n{}\nstderr:\n{}",
        manifest.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let plugin = target
        .join("debug")
        .join(format!("crikey-conformance-plugin{suffix}"));
    assert!(
        plugin.is_file(),
        "conformance plugin binary was not produced: {}",
        plugin.display()
    );
    plugin
});

fn conformance_binary() -> PathBuf {
    CONFORMANCE_BINARY.clone()
}

/// Escapes a path for a TOML basic string, including Windows separators.
fn toml_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character => escaped.push(character),
        }
    }
    escaped
}

/// Writes a normal native fixture with a mode selected by its working-dir file.
/// The provider supplies the package directory as the child's working
/// directory, where the conformance fixture reads this mode.
fn write_native_plugin(root: &Path, id: &str, binary: &Path, mode: &str) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir_all(&directory).expect("native plugin directory is creatable");

    let entrypoint = toml_string(&binary.to_string_lossy());
    let manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"{id}\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint = \"{entrypoint}\"\n"
    );
    fs::write(directory.join("crikey.toml"), manifest).expect("native manifest is writable");
    fs::write(directory.join("conformance-mode"), mode).expect("native mode witness is writable");
    directory
}

/// The safe mode a repeatedly failing install lands in.
fn safe_mode() -> StartupMode {
    StartupMode::SafeMode {
        consecutive_failures: SAFE_MODE_AFTER_FAILURES,
    }
}

/// Loads every admitted third-party root under `mode` through the real
/// provider and reports the plugins it brought up plus a description of the
/// packages it refused.
fn plugins_loaded_under(mode: &StartupMode, roots: &[PathBuf]) -> (Vec<PluginId>, Vec<String>) {
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load(&mut pipeline, &admitted_plugin_roots(mode, roots));
    let loaded = provider.plugins().to_vec();
    let unavailable = provider
        .unavailable()
        .iter()
        .map(|entry| format!("{entry:?}"))
        .collect::<Vec<_>>();
    provider.shutdown(NOW);
    (loaded, unavailable)
}

/// A first-party built-in plugin, which safe mode must *not* disable.
fn builtin() -> PluginId {
    PluginId("dev.crikey.builtin.apps".to_string())
}

/// Dispatch immediately, so the hand-driven query below needs no clock.
fn immediate_policy() -> PluginPolicy {
    PluginPolicy {
        profile: SchedulingProfile::Modern,
        debounce: DebouncePolicy {
            debounce_ms: 0,
            maximum_wait_ms: None,
            leading_edge: true,
            trailing_edge: true,
            minimum_query_length: 0,
        },
        ..PluginPolicy::modern()
    }
}

fn builtin_item(label: &str) -> Item {
    let plugin_id = builtin();
    let category = Category::Application;
    Item {
        stable_id: ItemId::derived(&plugin_id, &category, label),
        plugin_id,
        category,
        label: label.to_string(),
        description: "a first-party result".to_string(),
        target: label.to_string(),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: std::collections::BTreeMap::new(),
        actions: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The guarantee: safe mode means the packages are not loaded
// ---------------------------------------------------------------------------

/// One native package on disk, two startup modes, one test.
///
/// Under `Normal` the real provider must load it; under `SafeMode` the
/// identical bytes must produce a provider with zero plugins. Asserting both
/// in the same test is what makes this non-vacuous: a provider that always
/// loads nothing fails the normal half, and a safe mode implemented as a
/// cosmetic flag fails the safe half. Nothing about the package changes
/// between the runs — only the mode.
#[test]
fn the_same_native_package_loads_in_normal_mode_and_loads_not_at_all_in_safe_mode() {
    let conformance = conformance_binary();
    let scratch = Scratch::new("suppression");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "healthy", &conformance, "echo");
    let roots = vec![plugins_root];
    let healthy = native_plugin("healthy");

    let (normal_loaded, normal_unavailable) = plugins_loaded_under(&StartupMode::Normal, &roots);
    assert!(
        normal_loaded.contains(&healthy),
        "a normal startup must load the third-party native package; unavailable: {normal_unavailable:?}",
    );
    assert!(
        normal_unavailable.is_empty(),
        "the package is healthy, so nothing may be recorded unavailable: {normal_unavailable:?}",
    );

    let (safe_loaded, _) = plugins_loaded_under(&safe_mode(), &roots);
    assert_eq!(
        safe_loaded.len(),
        0,
        "safe mode must load no third-party plugin at all, loaded: {safe_loaded:?}",
    );

    assert!(
        !normal_loaded.is_empty() && safe_loaded.is_empty(),
        "the two runs must differ: normal loaded {normal_loaded:?}, safe mode loaded {safe_loaded:?}",
    );
}

/// Safe mode is not "CriKey stops working".
///
/// Spec 24.2 disables *third-party* plugins. With a safe-mode provider holding
/// zero native packages, the pipeline must still register, dispatch, rank and
/// present a first-party plugin's rows. Kills the fix where safe mode is
/// implemented by refusing to run queries, and the subtler one where the
/// suppressed package is still registered with the scheduler and shows up as a
/// dispatch that never answers.
#[test]
fn safe_mode_suppresses_third_party_plugins_without_disabling_the_pipeline_itself() {
    let conformance = conformance_binary();
    let scratch = Scratch::new("pipeline-alive");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "healthy", &conformance, "echo");
    let roots = vec![plugins_root];

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mode = safe_mode();
    let mut provider = NativeProvider::load(&mut pipeline, &admitted_plugin_roots(&mode, &roots));
    assert!(
        provider.plugins().is_empty(),
        "precondition: safe mode loaded no third-party plugin, found {:?}",
        provider.plugins(),
    );

    let builtin = builtin();
    pipeline
        .register_plugin(builtin.clone(), immediate_policy())
        .expect("a first-party plugin still registers in safe mode");

    let generation: Generation = pipeline.keystroke("report", NOW);
    let tick = pipeline.tick(NOW);
    assert_eq!(
        tick.dispatches.len(),
        1,
        "only the first-party plugin may be dispatched; the suppressed package must not be \
         registered at all, dispatches: {:?}",
        tick.dispatches
            .iter()
            .map(|request| request.plugin.clone())
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        tick.dispatches[0].plugin, builtin,
        "the single dispatch is the built-in plugin",
    );

    pipeline
        .deliver(
            ResultBatch {
                generation,
                plugin: builtin.clone(),
                state: BatchState::Final,
                items: vec![builtin_item("Reporter")],
            },
            NOW,
        )
        .expect("a first-party batch is admitted in safe mode");
    assert_eq!(
        pipeline.complete(&builtin, generation, NOW),
        CompletionOutcome::Accepted,
    );

    let frame = pipeline
        .present(NOW)
        .expect("safe mode still presents a frame for the current query");
    assert_eq!(
        frame.generation, generation,
        "the presented frame is the query the user just typed",
    );
    assert_eq!(
        frame.rows.len(),
        1,
        "the first-party row must survive safe mode, rows: {:?}",
        frame.rows.iter().map(|row| row.label.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(frame.rows[0].label, "Reporter");
    assert_eq!(
        frame.rows[0].plugin_name, builtin.0,
        "the surviving row belongs to the first-party plugin",
    );

    provider.shutdown(NOW);
}
