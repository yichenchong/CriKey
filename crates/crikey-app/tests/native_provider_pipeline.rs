//! Red-first live native-provider tests (contract §6; spec 5.2, 13.8, 19.4,
//! 24.1; acceptance 31.7, 31.8, 31.9, 31.21, 31.23).
//!
//! These tests deliberately drive the real out-of-tree conformance executable
//! through [`NativeProvider`] and [`NativeDriver`]. They do not substitute an
//! in-process fake: native code must cross the supervised process boundary,
//! bounded query intake and presented [`ViewModel`].

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crikey_app::{
    DisabledPlugins, NativeDriver, NativeProvider, PipelineConfig, PluginActionRouter, QueryPipeline,
    PLUGIN_ICON_DEADLINE,
};
use crikey_core::{ActionId, Generation, ItemId, PluginId};
use crikey_ui::ViewModel;

/// Collection window for tests whose subject is that a worker's rows reach the
/// pipeline at all, rather than how quickly.
///
/// The production window is 100 ms
/// ([`crikey_app::native_provider::DEFAULT_COLLECTION_WINDOW`]), which a real
/// subprocess round-trip can miss on a loaded machine — that made these tests
/// fail intermittently for a reason none of them asserts. This is a liveness
/// ceiling, not a latency assertion: a worker that never answers still fails
/// the test, just with a message instead of a race. Tests about the window
/// itself must use `NativeProvider::load`.
const ROW_DELIVERY_WINDOW: Duration = Duration::from_secs(5);

/// Liveness ceiling for a publication that waits on the **resource** child
/// rather than the query child.
///
/// Separate from [`ROW_DELIVERY_WINDOW`] because it bounds a strictly larger
/// cost. `[performance] startup = "eager"` starts a package's query
/// supervisor at load; the icon resolver installs a *second* supervisor whose
/// child is spawned on first use, so the first icon of a run also pays a cold
/// process spawn and handshake. Five seconds covered that on Linux and
/// Windows and did not on macOS, which is the whole reason this is its own
/// number.
///
/// Its value asserts nothing. The latency contract is
/// [`ICON_ISOLATION_WINDOW`] and [`BATCH_PUBLICATION_SLACK`], measured
/// separately and deliberately tight; this only decides how long a genuinely
/// stuck run takes to say so, and a passing run never waits it out.
const RESOURCE_DELIVERY_WINDOW: Duration = Duration::from_secs(60);

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
            "crikey-native-provider-{label}-{}-{}",
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

/// Builds the out-of-tree conformance workspace once and returns both binaries.
/// Cargo's own lock serializes concurrent integration-test processes.
fn conformance_binaries() -> (PathBuf, PathBuf) {
    static BINARIES: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

    BINARIES
        .get_or_init(|| {
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
            let misbehaving = target
                .join("debug")
                .join(format!("crikey-misbehaving-plugin{suffix}"));
            assert!(
                plugin.is_file(),
                "conformance plugin binary was not produced: {}",
                plugin.display()
            );
            assert!(
                misbehaving.is_file(),
                "misbehaving conformance binary was not produced: {}",
                misbehaving.display()
            );
            (plugin, misbehaving)
        })
        .clone()
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

/// Writes one native package manifest and, when requested, the fixture's mode
/// file. The clean absolute executable path is intentionally left as a single
/// manifest entrypoint; the provider supplies the package directory as the
/// child's working directory, where the conformance fixture reads this mode.
fn write_native_manifest(
    root: &Path,
    id: &str,
    entrypoint: Option<&Path>,
    mode: Option<&str>,
    minimum_query_length: Option<usize>,
) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir_all(&directory).expect("native plugin directory is creatable");

    let entrypoint = entrypoint
        .map(|path| format!("entrypoint = \"{}\"\n", toml_string(&path.to_string_lossy())))
        .unwrap_or_default();
    let activation = minimum_query_length
        .map(|minimum| format!("\n[activation]\nminimum-query-length = {minimum}\n"))
        .unwrap_or_default();
    let manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"{id}\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         {entrypoint}{activation}"
    );
    fs::write(directory.join("crikey.toml"), manifest).expect("native manifest is writable");
    if let Some(mode) = mode {
        fs::write(directory.join("conformance-mode"), mode).expect("native mode witness is writable");
    }
    directory
}

/// Writes a normal native fixture with a mode selected by its working-dir file.
fn write_native_plugin(root: &Path, id: &str, binary: &Path, mode: &str) -> PathBuf {
    write_native_manifest(root, id, Some(binary), Some(mode), None)
}

/// A bounded poll used only for observable child-process state, never to order
/// two test actions. This follows the supervised-worker tests' hard cap.
fn wait_for_process_table_absence(path: &Path) -> bool {
    for _ in 0..1_000 {
        if process_table_contains_working_dir(path) != Some(true) {
            return true;
        }
        sleep(Duration::from_millis(5));
    }
    false
}

/// Whether Linux still has a process whose current directory is `path`.
///
/// The non-Linux arm is a compiling counterpart: the app tests still exercise
/// the portable shutdown call, while process-table inspection is unavailable
/// without a platform-specific dependency on those targets.
fn process_table_contains_working_dir(path: &Path) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        let directory = fs::canonicalize(path).ok()?;
        let entries = fs::read_dir("/proc").ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            if fs::read_link(entry.path().join("cwd")).ok().as_deref() == Some(directory.as_path()) {
                return Some(true);
            }
        }
        Some(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

#[test]
fn native_suggestions_cross_pipeline_intake_under_current_generation() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("intake");
    let plugins_root = scratch.subdir("plugins");
    let package = write_native_plugin(&plugins_root, "healthy", &conformance, "echo");
    let healthy = native_plugin("healthy");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );

    assert!(
        provider.plugins().contains(&healthy),
        "the healthy native plugin must load and register; unavailable: {:?}",
        provider.unavailable(),
    );

    let frame = provider
        .drive_query(&mut pipeline, "report", 17)
        .expect("the admitted current native batch produces a frame");
    assert_eq!(
        pipeline.visible_generation(),
        Some(frame.generation),
        "the presented frame is the current generation",
    );
    assert!(
        pipeline.intake_diagnostics().admitted() >= 1,
        "the native batch must be admitted to intake",
    );
    assert!(
        pipeline.intake_diagnostics().merged() >= 1,
        "the native batch must be merged out of intake",
    );
    assert!(
        frame.rows.iter().any(|row| row.plugin_name == healthy.0),
        "the presented frame must carry the native plugin's suggestion, found: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );
    assert!(
        frame.rows.iter().any(|row| row.label.contains("report")),
        "the native conformance item must reach the frame with the query label, found: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );

    // The process cwd is the package directory, not the test's cwd. This is
    // also the witness used by the dedicated shutdown test below.
    if let Some(is_present) = process_table_contains_working_dir(&package) {
        assert!(
            is_present,
            "the native child runs with its package as working_dir"
        );
    }
    provider.shutdown(180);
}

#[test]
fn native_distinct_source_dirs_use_their_own_workers() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("identity");
    let plugins_root = scratch.subdir("plugins");

    // The executable and manifest entrypoint are identical. Only each package's
    // source directory and mode witness differ; sharing one worker would make
    // both plugins emit the same number of rows (contract §6 identity pin).
    write_native_plugin(&plugins_root, "alpha", &conformance, "stream:1");
    write_native_plugin(&plugins_root, "beta", &conformance, "stream:2");

    let alpha = native_plugin("alpha");
    let beta = native_plugin("beta");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&alpha) && provider.plugins().contains(&beta),
        "both native plugins must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let frame = provider
        .drive_query(&mut pipeline, "identity", 17)
        .expect("both native plugins produce a frame");
    let alpha_rows = frame.rows.iter().filter(|row| row.plugin_name == alpha.0).count();
    let beta_rows = frame.rows.iter().filter(|row| row.plugin_name == beta.0).count();
    assert_eq!(
        alpha_rows,
        1,
        "alpha's package-local mode must produce exactly one item; rows: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        beta_rows,
        2,
        "beta's package-local mode must produce exactly two items, not alpha's worker output; rows: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );

    provider.shutdown(180);
}

#[test]
fn native_router_rejects_duplicate_stable_ids_across_plugin_owners() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("duplicate-action-id");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "alpha", &conformance, "same-id");
    write_native_plugin(&plugins_root, "beta", &conformance, "same-id");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );
    assert_eq!(provider.plugins().len(), 2);
    provider
        .drive_query(&mut pipeline, "duplicate", 17)
        .expect("both duplicate-id plugins must publish their current snapshots");

    let driver = NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
    let mut router = PluginActionRouter::default();
    router
        .register(driver.plugins(), driver.action_executor())
        .expect("the router registers each native owner exactly once");
    let error = router
        .submit_by_item_id(&ItemId("echo-1".to_owned()), &ActionId("open".to_owned()), None)
        .expect_err("a stable id shared by two owners must not be routed arbitrarily");
    assert!(
        error.to_string().contains("ambiguous ownership"),
        "duplicate stable ids must be rejected explicitly: {error}"
    );
    drop(driver);
}

#[test]
fn native_worker_crash_is_contained_and_a_sibling_keeps_serving() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("crash");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "healthy", &conformance, "echo");
    write_native_plugin(&plugins_root, "crashy", &conformance, "crash-on-suggest");

    let healthy = native_plugin("healthy");
    let crashy = native_plugin("crashy");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&healthy) && provider.plugins().contains(&crashy),
        "both native plugins load before the runtime crash; unavailable: {:?}",
        provider.unavailable(),
    );

    for iteration in 0..5 {
        let frame = provider.drive_query(
            &mut pipeline,
            &format!("report {iteration}"),
            100 * (iteration + 1),
        );
        assert!(
            frame.as_ref().is_some_and(|frame| {
                frame.rows.iter().any(|row| row.plugin_name == healthy.0)
                    && !frame.rows.iter().any(|row| row.plugin_name == crashy.0)
            }),
            "the healthy sibling keeps serving and the crashed plugin contributes no rows on query {iteration}"
        );
    }

    // Crash teardown is asynchronous to the provider's bounded collection
    // window; wait only for the observed diagnostic, with a hard cap.
    for _ in 0..2_000 {
        if provider
            .dispatch_failures()
            .iter()
            .any(|(plugin, _)| plugin == &crashy)
        {
            break;
        }
        sleep(Duration::from_millis(5));
    }

    let crashy_failures = provider
        .dispatch_failures()
        .iter()
        .filter(|(plugin, _)| plugin == &crashy)
        .count();
    assert_eq!(
        crashy_failures,
        1,
        "a crashed native plugin's dispatch failure is recorded once, not per keystroke; failures: {:?}",
        provider.dispatch_failures(),
    );
    let crash_reason = provider
        .dispatch_failures()
        .iter()
        .find(|(plugin, _)| plugin == &crashy)
        .map(|(_, reason)| reason);
    assert!(
        crash_reason.is_some_and(|reason| !reason.trim().is_empty()),
        "the runtime crash has an attributable reason"
    );
    assert!(
        !provider
            .unavailable()
            .iter()
            .any(|entry| entry.plugin.as_ref() == Some(&crashy)),
        "runtime crashes belong in dispatch_failures, not load-time NativeUnavailable"
    );

    provider.shutdown(180);
}

#[test]
fn native_unavailable_packages_are_recorded_without_aborting_load() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("unavailable");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "healthy", &conformance, "echo");

    // No entrypoint for this target: a keyed map with neither the host's
    // os-arch nor the fallback `any` key.
    let no_entrypoint = plugins_root.join("no-entrypoint");
    fs::create_dir_all(&no_entrypoint).expect("no-entrypoint directory is creatable");
    fs::write(
        no_entrypoint.join("crikey.toml"),
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"no-entrypoint\"\n\
         name = \"no-entrypoint\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint = { \"unsupported-os-unsupported-arch\" = \"bin/plugin\" }\n",
    )
    .expect("no-entrypoint manifest is writable");

    // Entrypoint exists in the manifest but points at no binary.
    let missing_binary = scratch.join("missing-native-binary");
    assert!(!missing_binary.exists(), "missing-binary witness must be absent");
    write_native_manifest(&plugins_root, "missing-binary", Some(&missing_binary), None, None);

    // A parse failure is an unavailable package, not an aborted discovery pass.
    let malformed = plugins_root.join("malformed");
    fs::create_dir_all(&malformed).expect("malformed directory is creatable");
    fs::write(malformed.join("crikey.toml"), "[plugin\nid = \"broken\"\n")
        .expect("malformed manifest is writable");

    let healthy = native_plugin("healthy");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load(&mut pipeline, &[plugins_root], &DisabledPlugins::default());
    assert!(
        provider.plugins().contains(&healthy),
        "a healthy sibling still loads around unavailable packages; unavailable: {:?}",
        provider.unavailable(),
    );

    for package in ["no-entrypoint", "missing-binary", "malformed"] {
        let entries = provider
            .unavailable()
            .iter()
            .filter(|entry| entry.package == package)
            .collect::<Vec<_>>();
        assert_eq!(
            entries.len(),
            1,
            "package {package} becomes exactly one NativeUnavailable entry; unavailable: {:?}",
            provider.unavailable(),
        );
        assert!(
            !entries[0].reason.trim().is_empty(),
            "package {package} has an actionable unavailable reason"
        );
    }

    provider.shutdown(180);
}

#[test]
fn native_plugin_is_scheduled_under_its_manifest_query_policy() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("policy");
    let plugins_root = scratch.subdir("plugins");
    write_native_manifest(
        &plugins_root,
        "gated",
        Some(&conformance),
        Some("stream:1"),
        Some(5),
    );

    let gated = native_plugin("gated");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&gated),
        "the gated native plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let short = provider.drive_query(&mut pipeline, "hi", 17);
    assert!(
        short.is_none_or(|frame| !frame.rows.iter().any(|row| row.plugin_name == gated.0)),
        "a query below minimum-query-length must not dispatch the native plugin"
    );

    let long = provider
        .drive_query(&mut pipeline, "hello there", 5_000)
        .expect("a query above minimum-query-length produces a frame");
    assert!(
        long.rows.iter().any(|row| row.plugin_name == gated.0),
        "a query above the manifest minimum must dispatch the native plugin; rows: {:?}",
        long.rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );

    provider.shutdown(180);
}

/// The live half of "a slow sibling never delays a healthy plugin": driving a
/// real query against a real pair of subprocesses returns without waiting for
/// the slow one's call deadline.
///
/// Deliberately scoped to the deadline. Whether the healthy sibling *wins* the
/// provider's 100 ms collection window is a property of the host's scheduler —
/// on a loaded machine a real subprocess round-trip can lose that race, and
/// asserting it here made this test fail intermittently under full-suite CPU
/// contention. The "healthy rows are presented while the slow plugin is still
/// running" invariant (§31.3, §31.8) is proven deterministically over virtual
/// time by
/// `scheduling_pipeline::fast_plugin_rows_are_presented_while_the_slow_plugin_is_still_running`,
/// which is where that assertion belongs.
///
/// This is not vacuous: a provider that blocked on the slow sibling would take
/// its full 750 ms and would carry its rows, so both assertions below fail.
#[test]
fn native_query_returns_without_waiting_for_a_slow_sibling() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("slow-sibling");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "healthy", &conformance, "echo");
    write_native_plugin(&plugins_root, "slow", &conformance, "slow:750");

    let slow = native_plugin("slow");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load(&mut pipeline, &[plugins_root], &DisabledPlugins::default());
    assert_eq!(
        provider.unavailable(),
        &[],
        "both latency-test plugins must load: {:?}",
        provider.unavailable(),
    );

    let started = Instant::now();
    let frame = provider
        .drive_query(&mut pipeline, "latency", 17)
        .expect("the provider publishes a current frame");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(400),
        "a slow sibling must not hold the query until its 750ms call deadline (elapsed {elapsed:?})"
    );
    assert!(
        frame.rows.iter().all(|row| row.plugin_name != slow.0),
        "the slow sibling cannot have contributed rows to a frame that did not wait for it: {:?}",
        frame
            .rows
            .iter()
            .map(|row| (row.plugin_name.clone(), row.label.clone()))
            .collect::<Vec<_>>(),
    );

    provider.shutdown(180);
}

#[test]
fn native_manifest_hard_deadline_limits_suggest_call() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("manifest-timeout");
    let plugins_root = scratch.subdir("plugins");
    let package = write_native_plugin(&plugins_root, "timeout", &conformance, "slow:750");
    let manifest_path = package.join("crikey.toml");
    let mut manifest = fs::read_to_string(&manifest_path).expect("manifest is readable");
    manifest.push_str("\n[performance]\nsuggest-hard-timeout-ms = 100\n");
    fs::write(&manifest_path, manifest).expect("manifest deadline is writable");

    let timeout_plugin = native_plugin("timeout");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        Duration::from_secs(2),
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&timeout_plugin),
        "the timeout plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let started = Instant::now();
    let frame = provider
        .drive_query(&mut pipeline, "timeout", 17)
        .expect("a timed-out plugin still yields a current frame");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the manifest hard deadline must stop suggest near 100 ms, not the 5 s transport cap (elapsed {:?})",
        started.elapsed(),
    );
    assert!(
        !frame.rows.iter().any(|row| row.plugin_name == timeout_plugin.0),
        "a timed-out plugin must contribute no rows",
    );

    provider.shutdown(0);
}

#[test]
fn native_driver_refuses_a_superseded_generation_without_blocking_submit() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("supersede");
    let plugins_root = scratch.subdir("plugins");
    // This fixture deliberately ignores cancellation, so the provider thread
    // has one call in flight while the mailbox receives several replacements.
    write_native_plugin(&plugins_root, "slow", &conformance, "ignore-cancel:500");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load(&mut pipeline, &[plugins_root], &DisabledPlugins::default());
    assert!(
        provider.plugins().contains(&native_plugin("slow")),
        "the slow native plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let published: Arc<Mutex<Vec<Generation>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |frame: &ViewModel| {
            sink.lock()
                .expect("the publish sink is not poisoned")
                .push(frame.generation);
        }),
    );
    assert!(
        driver.has_plugins(),
        "the native supervisor must serve the loaded plugin"
    );

    let older = Generation::from_raw(1);
    let first_newer = Generation::from_raw(2);
    let second_newer = Generation::from_raw(3);
    let newest = Generation::from_raw(4);
    let submit_started = Instant::now();
    driver.submit(older, "report old", 17, Vec::new(), false, 0);
    let first_submit_elapsed = submit_started.elapsed();
    assert!(
        first_submit_elapsed < Duration::from_millis(100),
        "submitting a query must not wait on a slow native child (elapsed {first_submit_elapsed:?})"
    );

    let mut observed_in_flight = false;
    for _ in 0..10_000 {
        if driver.is_busy() {
            observed_in_flight = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        observed_in_flight,
        "the first query must be observed in flight before mailbox replacement"
    );

    driver.submit(first_newer, "report first", 18, Vec::new(), false, 0);
    driver.submit(second_newer, "report second", 19, Vec::new(), false, 0);
    driver.submit(newest, "report newest", 20, Vec::new(), false, 0);
    assert!(
        driver.pending_replacements() >= 1,
        "a single-slot mailbox replaces an older pending job when more than its capacity is submitted"
    );

    let mut seen_newest = false;
    for _ in 0..2_000 {
        if published
            .lock()
            .expect("the publish sink is not poisoned")
            .contains(&newest)
        {
            seen_newest = true;
            break;
        }
        sleep(Duration::from_millis(5));
    }
    assert!(seen_newest, "the newest native generation must be presented");

    let generations = published
        .lock()
        .expect("the publish sink is not poisoned")
        .clone();
    assert!(
        generations.iter().all(|generation| *generation == newest),
        "only the newest generation is presented; saw {generations:?}"
    );

    drop(driver);
}

#[test]
fn native_shutdown_reaps_every_child() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("shutdown");
    let plugins_root = scratch.subdir("plugins");
    let packages = [
        write_native_plugin(&plugins_root, "healthy-a", &conformance, "echo"),
        write_native_plugin(&plugins_root, "healthy-b", &conformance, "echo"),
    ];

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load(&mut pipeline, &[plugins_root], &DisabledPlugins::default());
    provider
        .drive_query(&mut pipeline, "shutdown", 17)
        .expect("the children are alive and serve before shutdown");

    let states = packages
        .iter()
        .map(|package| process_table_contains_working_dir(package))
        .collect::<Vec<_>>();
    let can_inspect = states.iter().all(|state| state.is_some());
    if can_inspect {
        for (package, state) in packages.iter().zip(states) {
            assert_eq!(
                state,
                Some(true),
                "the native child for {} is observable before shutdown",
                package.display()
            );
        }
    }

    provider.shutdown(180);
    if can_inspect {
        for package in &packages {
            assert!(
                wait_for_process_table_absence(package),
                "NativeProvider::shutdown reaps every child; package cwd remains in the process table: {}",
                package.display()
            );
        }
    }
    // On targets without a portable process-table query, the portable shutdown
    // call above remains exercised; Linux provides the orphan-sensitive proof.
}

#[test]
fn native_plugin_icons_round_trip_through_resource_request() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("icons");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "icons", &conformance, "icon");
    let owner = native_plugin("icons");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(provider.plugins().contains(&owner));

    // One query must produce the initial row and, after the supervised child
    // answers ResourceRequest, a later driver refresh must publish the icon.
    // No second query is submitted: the refresh is the only re-presentation.
    /// One observed publication: its generation, whether the served icon row
    /// was present, and that icon's dimensions with its first pixel.
    type IconObservation = (Generation, bool, Option<(u32, u32, [u8; 4])>);
    let published: Arc<Mutex<Vec<IconObservation>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |frame: &ViewModel| {
            let icon = frame.rows.iter().find(|row| {
                row.plugin_name == owner.0 && row.icon_reference.as_deref() == Some("served.svg")
            });
            let pixels = icon.and_then(|row| {
                row.icon.as_ref().and_then(|image| {
                    let rgba = image.rgba();
                    (rgba.len() >= 4).then(|| {
                        (
                            image.width(),
                            image.height(),
                            [rgba[0], rgba[1], rgba[2], rgba[3]],
                        )
                    })
                })
            });
            sink.lock().expect("the publish sink is not poisoned").push((
                frame.generation,
                icon.is_some(),
                pixels,
            ));
        }),
    );
    driver.submit(Generation::from_raw(1), "icon", 17, Vec::new(), false, 0);

    // This wait includes the resource child's first spawn, exactly as the
    // slow-icon test's decorated wait does, so it takes the same ceiling. The
    // hand-rolled bound this replaces expired into the `expect` below, which
    // then blamed a missing publication for a wait that had simply run out.
    await_condition(
        RESOURCE_DELIVERY_WINDOW,
        "a publication carrying decoded resource pixels",
        || {
            published
                .lock()
                .expect("the publish sink is not poisoned")
                .iter()
                .any(|(_, has_icon_row, pixels)| *has_icon_row && pixels.is_some())
        },
    );

    let entries = published
        .lock()
        .expect("the publish sink is not poisoned")
        .clone();
    assert!(
        entries.iter().any(|(_, has_icon_row, _)| *has_icon_row),
        "the native row carries the resource reference: {entries:?}"
    );
    let pixels = entries
        .iter()
        .find_map(|(_, _, pixels)| *pixels)
        .expect("the driver refresh must publish decoded ResourceRequest pixels");
    assert_eq!((pixels.0, pixels.1), (48, 48));
    assert_eq!(pixels.2, [0x33, 0x66, 0xcc, 0xff]);
    assert!(
        entries
            .iter()
            .all(|(generation, _, _)| *generation == Generation::from_raw(1)),
        "only the submitted generation is presented: {entries:?}"
    );
    drop(driver);
}
#[test]
fn native_healthy_worker_survives_repeated_queries() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("repeat");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "healthy", &conformance, "echo");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );

    for iteration in 0..3 {
        let frame = provider
            .drive_query(
                &mut pipeline,
                &format!("repeat {iteration}"),
                100 * (iteration + 1),
            )
            .expect("healthy native worker publishes every repeated query");
        assert_eq!(frame.rows.len(), 2);
        assert!(frame
            .rows
            .iter()
            .all(|row| row.plugin_name == native_plugin("healthy").0));
    }
    assert!(provider.dispatch_failures().is_empty());
    provider.shutdown(0);
}

/// Collection window for the icon-isolation tests.
///
/// Deliberately shorter than the fixture's 150 ms icon answer. The subject is
/// that an outstanding icon fetch costs the NEXT query nothing, and that is
/// only observable while the window is narrow enough that a fetch which had
/// consumed it would show up as a query with no rows at all.
///
/// Both packages start eagerly, so no measured QUERY pays child startup. That
/// says nothing about the icon: eager startup covers a package's query
/// supervisor, while the resource child behind an icon is spawned on first
/// use. Waits that include that spawn use [`RESOURCE_DELIVERY_WINDOW`].
const ICON_ISOLATION_WINDOW: Duration = Duration::from_millis(60);

/// How far past the collection window a batch may still be called on time.
///
/// Covers driver wake-up, one warm pipe round trip and frame assembly on a
/// loaded machine. It stays well below the fixture's 150 ms icon answer, so a
/// batch that waited for the icon can never pass for a batch that did not.
const BATCH_PUBLICATION_SLACK: Duration = Duration::from_millis(60);

/// Writes a native package whose worker is started at load time.
///
/// The icon-isolation tests measure one query against a short collection
/// window, and child startup is not what they are measuring.
fn write_eager_native_plugin(root: &Path, id: &str, binary: &Path, mode: &str) -> PathBuf {
    let directory = root.join(id);
    fs::create_dir_all(&directory).expect("native plugin directory is creatable");
    let manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"{id}\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint = \"{entrypoint}\"\n\n\
         [performance]\n\
         startup = \"eager\"\n",
        entrypoint = toml_string(&binary.to_string_lossy())
    );
    fs::write(directory.join("crikey.toml"), manifest).expect("native manifest is writable");
    fs::write(directory.join("conformance-mode"), mode).expect("native mode witness is writable");
    directory
}

/// One observed publication: when it reached the sink, its generation, whether
/// the plugin's row was in it, and whether that row carried decoded pixels.
type IconTiming = (Instant, Generation, bool, bool);

/// Spawns a driver that timestamps every publication mentioning `owner`.
fn spawn_icon_timing_driver(
    provider: NativeProvider,
    pipeline: QueryPipeline,
    owner: PluginId,
) -> (NativeDriver, Arc<Mutex<Vec<IconTiming>>>) {
    let published: Arc<Mutex<Vec<IconTiming>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |frame: &ViewModel| {
            let row = frame.rows.iter().find(|row| row.plugin_name == owner.0);
            sink.lock().expect("the publish sink is not poisoned").push((
                Instant::now(),
                frame.generation,
                row.is_some(),
                row.is_some_and(|row| row.icon.is_some()),
            ));
        }),
    );
    (driver, published)
}

/// Returns the first publication satisfying `predicate`, or fails by name.
///
/// A missing publication is the failure this reports, never a silent skip: the
/// defect these tests cover looks exactly like a row that never arrives.
///
/// `ceiling` is passed per call rather than taken from one constant because
/// the waits here do not bound the same work: a row comes from a query child
/// that is already running, an icon may also be waiting on a resource child
/// that has yet to spawn. One shared number is either too tight for the second
/// or uselessly loose for the first.
fn await_publication(
    published: &Mutex<Vec<IconTiming>>,
    ceiling: Duration,
    expectation: &str,
    predicate: impl Fn(&IconTiming) -> bool,
) -> IconTiming {
    let started = Instant::now();
    let deadline = started + ceiling;
    loop {
        let seen = published
            .lock()
            .expect("the publish sink is not poisoned")
            .clone();
        if let Some(entry) = seen.iter().copied().find(&predicate) {
            return entry;
        }
        assert!(
            Instant::now() < deadline,
            "no publication was {expectation} within {ceiling:?} (waited {:?}): {seen:?}",
            started.elapsed()
        );
        sleep(Duration::from_millis(2));
    }
}

/// Polls `ready` until it holds, or fails by name.
///
/// The counterpart to [`await_publication`] for sinks that are not
/// [`IconTiming`]. Hand-rolling `for _ in 0..N { sleep(..) }` instead loses two
/// things that matter when this fires on a machine nobody is sitting at: the
/// bound is spelled as an iteration count that has to be multiplied out to be
/// understood, and falling out of the loop leaves the *next* assertion to
/// report the failure, which then describes a missing value rather than a wait
/// that expired.
fn await_condition(ceiling: Duration, expectation: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < ceiling,
            "{expectation} did not happen within {ceiling:?}"
        );
        sleep(Duration::from_millis(2));
    }
}

/// A slow icon must not be paid for out of a query's collection window.
///
/// The fetch used to run on the same supervisor lock the query dispatcher
/// takes, so a plugin that spent its resource deadline answering an icon made
/// the next query time out with no rows at all — the plugin vanished because
/// one of its pictures was slow. The icon itself is decoration and is expected
/// late: a later refresh republishes the same generation once it lands.
#[test]
fn a_slow_icon_fetch_neither_delays_nor_displaces_the_next_result_batch() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("icon-slow");
    let plugins_root = scratch.subdir("plugins");
    let package = write_eager_native_plugin(&plugins_root, "slowicon", &conformance, "icon-slow");
    let owner = native_plugin("slowicon");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ICON_ISOLATION_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(provider.plugins().contains(&owner));
    let (driver, published) = spawn_icon_timing_driver(provider, pipeline, owner);

    // The first query is what puts the icon reference on a row, and the row is
    // what starts the fetch. Everything measured below happens while that
    // fetch is still outstanding.
    driver.submit(Generation::from_raw(1), "icon one", 17, Vec::new(), false, 0);
    let opening = await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a first-generation row",
        |(_, generation, row, _)| *generation == Generation::from_raw(1) && *row,
    );
    assert!(
        !opening.3,
        "the batch that first names the icon cannot already carry it"
    );

    let submitted = Instant::now();
    driver.submit(Generation::from_raw(2), "icon two", 200, Vec::new(), false, 0);
    let batch = await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a second-generation row",
        |(_, generation, row, _)| *generation == Generation::from_raw(2) && *row,
    );
    let batch_delay = batch.0.duration_since(submitted);
    assert!(
        batch_delay <= ICON_ISOLATION_WINDOW + BATCH_PUBLICATION_SLACK,
        "the batch waited {batch_delay:?} for a {ICON_ISOLATION_WINDOW:?} collection window"
    );
    assert!(
        !batch.3,
        "the on-time batch is published without the icon that has not arrived"
    );

    let decorated = await_publication(
        &published,
        // The only wait here that can include the resource child's first
        // spawn, because this is the first icon of the run.
        RESOURCE_DELIVERY_WINDOW,
        "a second-generation row carrying its icon",
        |(_, generation, _, icon)| *generation == Generation::from_raw(2) && *icon,
    );
    assert!(
        decorated.0 > batch.0,
        "the icon reaches the same generation by a later refresh, not by holding the batch"
    );
    let icon_delay = decorated.0.duration_since(submitted);
    assert!(
        icon_delay > ICON_ISOLATION_WINDOW,
        "the icon genuinely arrived after the collection window closed, not inside it"
    );

    // The icon arriving proves a resource child ran; teardown owes the same
    // reaping the query child gets. Both children share this working
    // directory, so its absence covers the pair.
    let observable = process_table_contains_working_dir(&package).is_some();
    drop(driver);
    if observable {
        assert!(
            wait_for_process_table_absence(&package),
            "the resource child is reaped with the provider; package cwd remains: {}",
            package.display()
        );
    }
}

/// A plugin that never answers a resource request must cost queries nothing.
///
/// Silence is bounded by the host's own icon deadline and by nothing the
/// plugin does, so the queries either side of it are published on time and the
/// abandoned request leaves nothing behind for a later query to queue behind.
#[test]
fn an_unanswered_icon_request_delays_no_query_and_leaves_nothing_outstanding() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("icon-silent");
    let plugins_root = scratch.subdir("plugins");
    write_eager_native_plugin(&plugins_root, "silenticon", &conformance, "icon-silent");
    let owner = native_plugin("silenticon");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ICON_ISOLATION_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(provider.plugins().contains(&owner));
    let (driver, published) = spawn_icon_timing_driver(provider, pipeline, owner);

    driver.submit(Generation::from_raw(1), "icon one", 17, Vec::new(), false, 0);
    await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a first-generation row",
        |(_, generation, row, _)| *generation == Generation::from_raw(1) && *row,
    );

    let submitted = Instant::now();
    driver.submit(Generation::from_raw(2), "icon two", 200, Vec::new(), false, 0);
    let during = await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a second-generation row",
        |(_, generation, row, _)| *generation == Generation::from_raw(2) && *row,
    );
    let during_delay = during.0.duration_since(submitted);
    assert!(
        during_delay <= ICON_ISOLATION_WINDOW + BATCH_PUBLICATION_SLACK,
        "a query issued while the request is unanswered waited {during_delay:?}"
    );

    // Past the host's own deadline the request is abandoned. Nothing may
    // remain of it that a later query could be made to queue behind.
    sleep(PLUGIN_ICON_DEADLINE + BATCH_PUBLICATION_SLACK);
    let resubmitted = Instant::now();
    driver.submit(Generation::from_raw(3), "icon three", 900, Vec::new(), false, 0);
    let after = await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a third-generation row",
        |(_, generation, row, _)| *generation == Generation::from_raw(3) && *row,
    );
    let after_delay = after.0.duration_since(resubmitted);
    assert!(
        after_delay <= ICON_ISOLATION_WINDOW + BATCH_PUBLICATION_SLACK,
        "a query issued after the abandoned request waited {after_delay:?}"
    );

    assert!(
        published
            .lock()
            .expect("the publish sink is not poisoned")
            .iter()
            .all(|(_, _, _, icon)| !*icon),
        "a reference the plugin never answers stays undecorated"
    );
    drop(driver);
}

/// A plugin that answers after the collection window still gets its rows shown.
///
/// [`NativeProvider::drive_query`] waits `DEFAULT_COLLECTION_WINDOW` (100 ms)
/// and then completes every request it gathered, so a slower plugin's call is
/// still running when the keystroke's frame is published. That answer arrives
/// on the completion channel a few hundred milliseconds later, and the driver's
/// periodic refresh is the only thing still running for that generation. If the
/// refresh does not retire it, the rows are dropped and the user never sees the
/// plugin's results at all — until they type another character, which is the
/// one thing that would throw the answer away again.
///
/// This is a liveness assertion, not a latency one. The bound is generous; what
/// it forbids is "never".
#[test]
fn a_native_answer_after_the_collection_window_still_reaches_the_user() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("late-answer");
    let plugins_root = scratch.subdir("plugins");
    // Comfortably past the 100 ms collection window and comfortably inside the
    // plugin's own call deadline, so the child really does answer.
    write_native_plugin(&plugins_root, "slow", &conformance, "slow:400");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load(&mut pipeline, &[plugins_root], &DisabledPlugins::default());
    assert_eq!(
        provider.unavailable(),
        &[],
        "the slow plugin must load: {:?}",
        provider.unavailable(),
    );

    let published: Arc<Mutex<Vec<(Generation, bool, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |frame: &ViewModel| {
            sink.lock().expect("the publish sink is not poisoned").push((
                frame.generation,
                frame.pending_plugins,
                frame.rows.len(),
            ));
        }),
    );

    let generation = Generation::from_raw(1);
    driver.submit(generation, "late", 17, Vec::new(), false, 0);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delivered = false;
    while Instant::now() < deadline {
        delivered = published
            .lock()
            .expect("the publish sink is not poisoned")
            .iter()
            .any(|(published_generation, _, rows)| *published_generation == generation && *rows >= 1);
        if delivered {
            break;
        }
        sleep(Duration::from_millis(10));
    }
    assert!(
        delivered,
        "the slow plugin's answer must reach a published frame without another keystroke; \
         frames seen (generation, pending, rows): {:?}",
        published.lock().expect("the publish sink is not poisoned"),
    );

    // And the frame carrying it must not still claim work is outstanding.
    let settled = published
        .lock()
        .expect("the publish sink is not poisoned")
        .iter()
        .rev()
        .find(|(published_generation, _, rows)| *published_generation == generation && *rows >= 1)
        .copied();
    let (_, pending, _) = settled.expect("the delivering frame was just observed");
    assert!(
        !pending,
        "the frame that finally carries the plugin's rows must not still say it is waiting for it"
    );

    drop(driver);
}

/// A plugin that answers "I failed" has still answered.
///
/// Its request must be retired, not left outstanding. A protocol failure is
/// reported inside the collection window and carries no rows, so it looks from
/// the outside exactly like a plugin that has not replied yet — and a request
/// left active for a call that already finished can never be retired by
/// anything, because the completion it was waiting for has been consumed. The
/// launcher would say "Providers are still responding" until the user typed
/// again.
#[test]
fn a_native_plugin_that_reports_failure_does_not_leave_the_query_outstanding() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("failed-batch");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "failing", &conformance, "fail-suggest");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );
    assert_eq!(
        provider.unavailable(),
        &[],
        "the failing plugin must load: {:?}",
        provider.unavailable(),
    );

    let frame = provider
        .drive_query(&mut pipeline, "anything", 17)
        .expect("the provider publishes a frame for the current generation");
    assert!(
        !frame.pending_plugins,
        "a plugin that reported a failure is not still being waited for"
    );
    assert!(
        !provider.dispatch_failures().is_empty(),
        "the failure must still be recorded as a diagnostic"
    );

    provider.shutdown(180);
}

/// A plugin busy with the previous keystroke's call gets no new call for this
/// one — and the request it never received must not be left outstanding.
///
/// `collect_suggestions` skips a plugin that already has a call in flight. That
/// older call cannot answer the newer generation: when it finishes it is
/// retired under the generation it belongs to. So nothing will ever arrive for
/// the newer request, and leaving it active strands the newest frame as
/// "Providers are still responding" forever. Typing fast into a launcher is the
/// normal case, and this plugin ignores cancellation, which is the whole point:
/// a plugin that misbehaves must not be able to wedge the UI.
#[test]
fn a_plugin_busy_with_an_older_call_does_not_strand_the_newest_query() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("busy-older");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "stubborn", &conformance, "ignore-cancel:500");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    // The production window: the point is that the first call is still running
    // when the second query arrives.
    let mut provider = NativeProvider::load(&mut pipeline, &[plugins_root], &DisabledPlugins::default());
    assert_eq!(
        provider.unavailable(),
        &[],
        "the stubborn plugin must load: {:?}",
        provider.unavailable(),
    );

    let _first = provider.drive_query(&mut pipeline, "first", 17);
    let second = provider
        .drive_query(&mut pipeline, "second", 18)
        .expect("the newer generation is presented");
    assert!(
        !second.pending_plugins,
        "the newest frame must not wait on a call that was never made for it"
    );

    provider.shutdown(180);
}
