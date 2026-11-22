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
    DisabledPlugins, NativeDriver, NativeProvider, PipelineConfig, PluginActionRouter, ProviderQueryContext,
    QueryPipeline,
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
/// Its value asserts nothing; it only bounds how long a genuinely stuck run
/// takes to say so. Isolation is witnessed by publication before an explicit
/// resource release, independently of driver/OS scheduling latency.
const RESOURCE_DELIVERY_WINDOW: Duration = Duration::from_secs(60);

/// Liveness ceiling for an answer that is *deliberately* late, from a plugin
/// whose child is also cold.
///
/// [`ROW_DELIVERY_WINDOW`] covers a warm round trip. This test's subject
/// arrives after the collection window on purpose, and the package it comes
/// from spawns its child on first query, so one run pays a process spawn, a
/// handshake and the fixture's own delay before the first row can exist. Five
/// seconds covered that everywhere except a loaded macOS runner, where it
/// expired while the production path was still inside the startup budget it is
/// allowed to take -- a test failing for being *quicker to give up* than the
/// code it tests is permitted to be.
///
/// Its value asserts nothing, and a passing run never waits it out.
const LATE_ANSWER_DELIVERY_WINDOW: Duration = Duration::from_secs(60);

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

/// A real supervised action must wake the host without another query or input.
/// Draining inside the callback also proves publication precedes notification
/// and the completion mailbox is unlocked when the callback runs.
#[test]
fn native_action_completion_wakes_without_another_query() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("action-wake");
    let plugins_root = scratch.subdir("plugins");
    write_native_plugin(&plugins_root, "page", &conformance, "page");
    let plugin = native_plugin("page");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ROW_DELIVERY_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(
        provider.plugins().contains(&plugin),
        "the native page fixture must load; unavailable: {:?}",
        provider.unavailable(),
    );
    let (published, publication_rx) = std::sync::mpsc::channel();
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |frame| {
            let _ = published.send(frame.clone());
        }),
    );
    driver.submit(Generation::from_raw(1), "page", 17, Vec::new(), false, 0);
    let frame = publication_rx
        .recv_timeout(ROW_DELIVERY_WINDOW)
        .expect("the native page item must reach the driver");
    assert!(frame.rows.iter().any(|row| row.plugin_name == plugin.0));

    let executor = driver.action_executor();
    let callback_executor = Arc::downgrade(&executor);
    let (notified, notification_rx) = std::sync::mpsc::channel();
    driver.set_action_wake(move || {
        let completions = callback_executor
            .upgrade()
            .expect("the driver retains the action endpoint during notification")
            .poll_plugin_actions();
        let _ = notified.send(completions);
    });
    let item_id = ItemId("page-playground".to_owned());
    let action_id = ActionId("open-page".to_owned());
    let request_id = executor
        .submit_plugin_action_by_id(&plugin, &item_id, &action_id, None)
        .expect("the native page action must be admitted");

    // No further query, input, or completion polling drives the supervisor.
    let mut completions = notification_rx
        .recv_timeout(ROW_DELIVERY_WINDOW)
        .expect("the supervised native action must notify the host");
    assert_eq!(completions.len(), 1, "notification must follow publication");
    let completion = completions.pop().unwrap();
    assert_eq!(completion.request_id, request_id);
    assert_eq!(completion.plugin, plugin);
    assert_eq!(completion.item_id, item_id);
    assert_eq!(completion.action_id, Some(action_id));
    assert_eq!(
        completion.outcome.expect("the native page action must succeed"),
        crikey_app::ActionEffect::ShowPage {
            page_id: "playground".to_owned(),
        },
    );
    assert!(
        executor.poll_plugin_actions().is_empty(),
        "the terminal outcome is emitted once",
    );
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
    let plugin_dir = write_native_plugin(&plugins_root, "slow", &conformance, "gated-ignore-cancel");
    let manifest = plugin_dir.join("crikey.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str("\n[performance]\nsuggest-hard-timeout-ms = 5000\n");
    fs::write(manifest, text).unwrap();
    let entered = plugin_dir.join("conformance-gate-entered");
    let release = plugin_dir.join("conformance-gate-release");
    let wait_for_callback = |expected: &str| {
        let deadline = Instant::now() + ROW_DELIVERY_WINDOW;
        loop {
            if fs::read_to_string(&entered).ok().as_deref() == Some(expected) {
                break;
            }
            assert!(Instant::now() < deadline, "callback {expected:?} never entered");
            std::thread::yield_now();
        }
    };

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load(&mut pipeline, &[plugins_root], &DisabledPlugins::default());
    assert!(
        provider.plugins().contains(&native_plugin("slow")),
        "the slow native plugin must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let (published, publication_rx) = std::sync::mpsc::channel();
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |frame: &ViewModel| {
            let _ = published.send(frame.clone());
        }),
    );
    assert!(
        driver.has_plugins(),
        "the native supervisor must serve the loaded plugin"
    );

    let older = Generation::from_raw(1);
    let newest = Generation::from_raw(7);
    let submit_started = Instant::now();
    driver.submit(older, "report old", 17, Vec::new(), false, 0);
    let first_submit_elapsed = submit_started.elapsed();
    assert!(
        first_submit_elapsed < Duration::from_millis(100),
        "submitting a query must not wait on a slow native child (elapsed {first_submit_elapsed:?})"
    );

    wait_for_callback("report old\n");
    // Observing the initial publication, while the callback is held at the
    // gate, proves it outlived the collection window rather than racing a sleep.
    let opening = publication_rx
        .recv_timeout(ROW_DELIVERY_WINDOW)
        .expect("the collection window publishes while the physical callback is occupied");
    assert_eq!(opening.generation, older);
    assert!(opening.pending_plugins);
    for generation in 2..=7 {
        driver.submit(
            Generation::from_raw(generation),
            &format!("report {generation}"),
            17 + generation,
            Vec::new(),
            false,
            0,
        );
    }
    let deadline = Instant::now() + ROW_DELIVERY_WINDOW;
    loop {
        let frame = publication_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the newest query publishes without waiting for the old callback");
        if frame.generation == newest {
            assert!(
                frame.pending_plugins,
                "an occupied worker must retain the newest query"
            );
            assert!(!frame
                .rows
                .iter()
                .any(|row| row.plugin_name == native_plugin("slow").0));
            break;
        }
    }
    fs::write(&release, "report old").expect("release the obsolete callback");
    wait_for_callback("report old\nreport 7\n");
    fs::write(&release, "report 7").expect("release the newest callback");
    let deadline = Instant::now() + ROW_DELIVERY_WINDOW;
    loop {
        let frame = publication_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the newest actual callback must contribute a fresh native row");
        assert_eq!(
            frame.generation, newest,
            "obsolete native frames must not publish"
        );
        if frame
            .rows
            .iter()
            .any(|row| row.plugin_name == native_plugin("slow").0 && row.label == "report 7 (gated)")
        {
            assert!(!frame.pending_plugins);
            break;
        }
    }

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
    // Inside the previous query's debounce interval, the direct API still
    // waits for real eligibility and returns this query's rows synchronously.
    let frame = provider
        .drive_query(&mut pipeline, "repeat debounced", 301)
        .expect("the synchronous provider waits out debounce");
    assert_eq!(frame.rows.len(), 2);
    assert!(frame
        .rows
        .iter()
        .all(|row| row.label.starts_with("repeat debounced")));
    assert!(!frame.pending_plugins);
    assert!(provider.dispatch_failures().is_empty());
    provider.shutdown(0);
}

/// Collection window for the icon-isolation tests.
///
/// Kept at 60 ms even when the real resource child is blocked. The isolation
/// witness asserts publication while that child remains held, rather than
/// conflating the collection budget with driver/OS scheduling latency.
const ICON_ISOLATION_WINDOW: Duration = Duration::from_millis(60);

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

/// A blocked resource worker must not prevent the next query's publication.
///
/// The resource child's startup is gated while its real supervisor mutex is
/// held. A second-generation row must reach the public sink before release;
/// a later refresh must decorate that same generation after release.
#[test]
fn a_slow_icon_fetch_neither_delays_nor_displaces_the_next_result_batch() {
    struct ReleaseResourceOnDrop(Option<PathBuf>);
    impl ReleaseResourceOnDrop {
        fn release(mut self) {
            fs::write(self.0.as_ref().unwrap(), b"continue").expect("resource release is writable");
            self.0 = None;
        }
    }
    impl Drop for ReleaseResourceOnDrop {
        fn drop(&mut self) {
            if let Some(path) = self.0.take() {
                let _ = fs::write(path, b"continue");
            }
        }
    }

    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("icon-slow");
    let plugins_root = scratch.subdir("plugins");
    let package = write_eager_native_plugin(&plugins_root, "slowicon", &conformance, "icon-gated");
    let owner = native_plugin("slowicon");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ICON_ISOLATION_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(provider.plugins().contains(&owner));
    let gate = scratch.subdir("resource-gate");
    // The eager query process has already started, so only the resource
    // process launched on the first icon fetch reads this startup gate.
    fs::write(package.join("icon-startup-gate"), gate.to_str().unwrap())
        .expect("resource startup gate is writable");
    let (driver, published) = spawn_icon_timing_driver(provider, pipeline, owner);
    // Declared after the driver so panic unwinding releases its child before
    // driver teardown joins the supervised resource work.
    let release = ReleaseResourceOnDrop(Some(gate.join("release")));

    // The first row starts the lazy, separately supervised resource child.
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

    await_condition(
        RESOURCE_DELIVERY_WINDOW,
        "the resource child entering its startup gate",
        || gate.join("entered").is_file(),
    );
    let resource_pid = fs::read_to_string(gate.join("entered")).expect("resource child named its PID");
    assert!(!resource_pid.is_empty());
    assert!(!gate.join("released").exists());
    driver.submit(Generation::from_raw(2), "icon two", 200, Vec::new(), false, 0);
    let batch = await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a second-generation row",
        |(_, generation, row, _)| *generation == Generation::from_raw(2) && *row,
    );
    assert!(
        !gate.join("released").exists(),
        "the batch arrived before resource release"
    );
    assert!(
        !batch.3,
        "the batch is published without the icon while the resource child is held"
    );

    // A post-publication challenge proves the same real child is still alive
    // and blocked: a timed-out/killed child or a replacement cannot satisfy
    // the PID-scoped witness. No wall-clock slack stands in for this ordering.
    fs::write(gate.join("probe"), b"still held?").expect("resource probe is writable");
    await_condition(
        ROW_DELIVERY_WINDOW,
        "the held resource child's reply to the probe",
        || fs::read_to_string(gate.join("held")).is_ok_and(|pid| pid == resource_pid),
    );
    assert!(!gate.join("released").exists());
    release.release();

    let decorated = await_publication(
        &published,
        // Retains the existing resource liveness bound, including startup.
        RESOURCE_DELIVERY_WINDOW,
        "a second-generation row carrying its icon",
        |(_, generation, _, icon)| *generation == Generation::from_raw(2) && *icon,
    );
    assert!(
        decorated.0 > batch.0,
        "the icon reaches the same generation by a later refresh, not by holding the batch"
    );
    assert_eq!(
        fs::read_to_string(gate.join("released")).expect("resource startup was released"),
        resource_pid,
        "the same isolated child was released before its icon decorated generation two"
    );
    assert_eq!(
        fs::read_to_string(gate.join("served")).expect("the icon server named its PID"),
        resource_pid,
        "the first resource owner, not a replacement, supplied the decoded icon"
    );

    // The icon arriving proves a resource child ran; teardown owes the same
    // reaping the query child gets. Both children share this working
    // directory, so its absence covers the pair.
    let observable = process_table_contains_working_dir(&package).is_some();
    drop(driver);
    let children =
        fs::read_to_string(package.join("icon-child-pids")).expect("physical child receipts are readable");
    let children: Vec<u32> = children
        .lines()
        .map(|pid| pid.parse().expect("physical PID is numeric"))
        .collect();
    assert_eq!(
        children.len(),
        2,
        "only the eager query and first resource child ran"
    );
    assert_ne!(
        children[0], children[1],
        "query and resource must be physically isolated"
    );
    assert_eq!(
        children[1].to_string(),
        resource_pid,
        "the first resource owner was not replaced"
    );
    await_condition(
        ROW_DELIVERY_WINDOW,
        "both SDK servers returning successfully",
        || {
            children
                .iter()
                .all(|pid| package.join(format!("icon-exit-{pid}")).is_file())
        },
    );
    let request = fs::read_to_string(gate.join("request-1"))
        .expect("the successful first icon request crossed the real transport");
    let fields: Vec<_> = request.split_whitespace().collect();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0], resource_pid);
    assert_ne!(fields[1].parse::<u64>().expect("request ID is numeric"), 0);
    assert_eq!(fields[2], "served.svg");
    assert!(
        !gate.join("request-2").exists(),
        "the first fetch succeeded without a retry"
    );
    if observable {
        assert!(
            wait_for_process_table_absence(&package),
            "the resource child is reaped with the provider; package cwd remains: {}",
            package.display()
        );
    }
}

/// An unanswered resource must not displace current query rows.
///
/// Wire receipt of a distinct retry proves that the serialized first host
/// fetch has returned without an answer. The original guest callback stays
/// unanswered until the host sends Shutdown; this does not claim its original
/// host wait is still pending. Driver teardown and the SDK stop receipt witness
/// normal settlement, including the latest retry active when teardown began.
#[test]
fn an_unanswered_icon_request_delays_no_query_and_leaves_nothing_outstanding() {
    struct ReleaseSilentOnDrop(PathBuf);
    impl Drop for ReleaseSilentOnDrop {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, b"return through SDK shutdown");
        }
    }

    fn request_witness(gate: &Path, attempt: usize, ceiling: Duration) -> (u32, u64) {
        let path = gate.join(format!("request-{attempt}"));
        await_condition(ceiling, "a real inbound resource request", || path.is_file());
        let record = fs::read_to_string(path).expect("complete request witness was published");
        let fields: Vec<_> = record.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[2], "never.svg", "every attempt serves the same icon key");
        let pid = fields[0].parse::<u32>().expect("physical PID is numeric");
        let request = fields[1].parse::<u64>().expect("request ID is numeric");
        assert_ne!(pid, 0);
        assert_ne!(request, 0);
        (pid, request)
    }

    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("icon-silent");
    let plugins_root = scratch.subdir("plugins");
    let package = write_eager_native_plugin(&plugins_root, "silenticon", &conformance, "icon-silent");
    let owner = native_plugin("silenticon");

    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[plugins_root],
        ICON_ISOLATION_WINDOW,
        &DisabledPlugins::default(),
    );
    assert!(provider.plugins().contains(&owner));
    let gate = scratch.subdir("silent-resource");
    fs::write(package.join("icon-silent-gate"), gate.to_str().unwrap())
        .expect("silent resource witness is configured after eager query startup");
    let published = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |frame| {
            let row = frame.rows.iter().find(|row| row.plugin_name == owner.0);
            if let Some(row) = row {
                assert_eq!(
                    row.label,
                    format!("icon-never-{}", frame.generation.get()),
                    "a current generation must never publish an earlier query's item"
                );
            }
            sink.lock().unwrap().push((
                Instant::now(),
                frame.generation,
                row.is_some(),
                row.is_some_and(|row| row.icon.is_some()),
            ));
        }),
    );
    // Release the callback before driver teardown even when an assertion panics.
    let release = ReleaseSilentOnDrop(gate.join("release"));
    driver.submit(Generation::from_raw(1), "icon one", 17, Vec::new(), false, 0);
    await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a first-generation row",
        |(_, generation, row, _)| *generation == Generation::from_raw(1) && *row,
    );
    let first = request_witness(&gate, 1, RESOURCE_DELIVERY_WINDOW);
    let resource_pid = first.0.to_string();
    let held = gate.join("held");
    driver.submit(Generation::from_raw(2), "icon two", 200, Vec::new(), false, 0);
    let during = await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a current second-generation row while the resource is unanswered",
        |(_, generation, row, _)| *generation == Generation::from_raw(2) && *row,
    );
    assert!(!during.3);
    fs::write(gate.join("probe"), b"still unanswered?").expect("silent callback probe is writable");
    await_condition(
        ROW_DELIVERY_WINDOW,
        "the original unanswered callback's probe",
        || fs::read_to_string(&held).is_ok_and(|pid| pid == resource_pid),
    );
    assert!(!gate.join("answered").exists());

    // The resource supervisor serializes requests: receipt of request two
    // proves fetch one abandoned its unanswered wait, without a test sleep.
    let second = request_witness(&gate, 2, ROW_DELIVERY_WINDOW);
    let third = request_witness(&gate, 3, ROW_DELIVERY_WINDOW);
    assert_eq!(first.0, second.0, "silence must not respawn the resource child");
    assert_eq!(first.0, third.0, "all bounded attempts use the original child");
    assert_ne!(first.1, second.1);
    assert_ne!(first.1, third.1);
    assert_ne!(second.1, third.1);
    driver.submit(Generation::from_raw(3), "icon three", 900, Vec::new(), false, 0);
    let after = await_publication(
        &published,
        ROW_DELIVERY_WINDOW,
        "a current third-generation row after the first fetch was abandoned",
        |(_, generation, row, _)| *generation == Generation::from_raw(3) && *row,
    );
    assert!(!after.3);
    assert!(!gate.join("request-4").exists());
    assert!(!gate.join("answered").exists());
    assert!(
        published.lock().unwrap().iter().all(|(_, _, _, icon)| !*icon),
        "an icon the guest never answers must never decorate any generation"
    );

    // This version closes admission through driver Drop. The transparent
    // reader releases the callback only after observing the real Shutdown;
    // the test must not release it before teardown has actually begun.
    let observable = process_table_contains_working_dir(&package).is_some();
    drop(driver);
    drop(release);
    let children =
        fs::read_to_string(package.join("icon-child-pids")).expect("physical child receipts are readable");
    let children: Vec<u32> = children
        .lines()
        .map(|pid| pid.parse().expect("physical PID is numeric"))
        .collect();
    assert_eq!(
        children.len(),
        2,
        "only the eager query and first resource child ran"
    );
    assert_ne!(
        children[0], children[1],
        "query and resource must be physically isolated"
    );
    assert_eq!(
        children[1].to_string(),
        resource_pid,
        "the first resource owner was not replaced"
    );
    await_condition(
        ROW_DELIVERY_WINDOW,
        "both SDK servers returning successfully",
        || {
            children
                .iter()
                .all(|pid| package.join(format!("icon-exit-{pid}")).is_file())
        },
    );
    assert!(!gate.join("request-4").exists());
    assert!(
        gate.join("answered").is_file(),
        "release must return through the SDK's real missing-resource response"
    );
    assert_eq!(
        fs::read_to_string(gate.join("shutdown")).expect("resource child received real Shutdown"),
        resource_pid
    );
    assert_eq!(
        fs::read_to_string(gate.join("stopped")).expect("resource child ran the SDK stop callback"),
        resource_pid
    );
    assert!(
        published.lock().unwrap().iter().all(|(_, _, _, icon)| !*icon),
        "responses after admission closes cannot decorate a retired generation"
    );
    if observable {
        assert!(
            wait_for_process_table_absence(&package),
            "both real children must be reaped"
        );
    }
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

    let deadline = Instant::now() + LATE_ANSWER_DELIVERY_WINDOW;
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

/// A physically occupied worker keeps the newest query pending, rather than
/// manufacturing an empty final answer. Retirement of the old callback must
/// admit that same newest generation on refresh, without another keystroke.
#[test]
fn a_plugin_busy_with_an_older_call_does_not_strand_the_newest_query() {
    let (conformance, _) = conformance_binaries();
    let scratch = Scratch::new("busy-older");
    let plugins_root = scratch.subdir("plugins");
    let plugin_dir = write_native_plugin(&plugins_root, "stubborn", &conformance, "gated-ignore-cancel");
    let manifest = plugin_dir.join("crikey.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    text.push_str("\n[performance]\nsuggest-hard-timeout-ms = 5000\n");
    fs::write(manifest, text).unwrap();
    let entered = plugin_dir.join("conformance-gate-entered");
    let release = plugin_dir.join("conformance-gate-release");
    let wait_for_callback = |expected: &str| {
        let deadline = Instant::now() + ROW_DELIVERY_WINDOW;
        loop {
            if fs::read_to_string(&entered).ok().as_deref() == Some(expected) {
                break;
            }
            assert!(Instant::now() < deadline, "callback {expected:?} never entered");
            std::thread::yield_now();
        }
    };

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

    let first = provider
        .drive_query(&mut pipeline, "first", 17)
        .expect("the first collection window publishes while its callback is held");
    assert!(first.pending_plugins);
    wait_for_callback("first\n");

    let (published, publication_rx) = std::sync::mpsc::channel();
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |frame| {
            let _ = published.send(frame.clone());
        }),
    );
    let newest = Generation::from_raw(2);
    driver.submit(newest, "second", 18, Vec::new(), false, 0);
    let second = publication_rx
        .recv_timeout(ROW_DELIVERY_WINDOW)
        .expect("the newer generation is presented while the old callback is held");
    assert_eq!(second.generation, newest);
    assert!(
        second.pending_plugins,
        "the newest query is pending until the worker is available"
    );
    assert!(!second
        .rows
        .iter()
        .any(|row| row.plugin_name == native_plugin("stubborn").0));

    fs::write(&release, "first").expect("release the old callback");
    wait_for_callback("first\nsecond\n");
    fs::write(&release, "second").expect("release the newest actual callback");
    let deadline = Instant::now() + ROW_DELIVERY_WINDOW;
    loop {
        let frame = publication_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the retained second query must publish its fresh native row");
        assert_eq!(frame.generation, newest);
        if frame
            .rows
            .iter()
            .any(|row| row.plugin_name == native_plugin("stubborn").0 && row.label == "second (gated)")
        {
            assert!(
                !frame.pending_plugins,
                "the completed newest query is no longer pending"
            );
            break;
        }
    }
    drop(driver);
}

fn typed_values(password: &str) -> crikey_core::structured_arguments::ArgumentValues {
    use crikey_core::structured_arguments::*;
    ArgumentValues(std::collections::BTreeMap::from([
        (
            FieldId("username".into()),
            StructuredValue::NonSecret(NonSecretValue::Text("operator".into())),
        ),
        (
            FieldId("password".into()),
            StructuredValue::Password(Password::new(password.to_owned()).unwrap()),
        ),
        (
            FieldId("region".into()),
            StructuredValue::NonSecret(NonSecretValue::Dropdown(OptionId("local".into()))),
        ),
    ]))
}

#[test]
fn real_native_typed_static_release_confirms_current_worker_and_excludes_history() {
    use crikey_core::structured_arguments::{ExecutableKey, OwnedItem, SubmissionOutcome};
    let scratch = Scratch::new("typed-static");
    let (binary, _) = conformance_binaries();
    write_native_plugin(&scratch.path, "typed", &binary, "typed");
    let owner = native_plugin("typed");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load(
        &mut pipeline,
        &[scratch.path.clone()],
        &DisabledPlugins::default(),
    );
    assert!(provider.unavailable().is_empty(), "{:?}", provider.unavailable());
    provider
        .request_catalog_build(&owner, 1, Generation::ZERO)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(result) = provider.take_catalog_results().into_iter().next() {
            assert!(
                matches!(result, crikey_app::CatalogBuildResult::Complete(_)),
                "{result:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "typed catalog did not finish");
        sleep(Duration::from_millis(2));
    }
    let driver = NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
    let executor = driver.action_executor();
    let key = ExecutableKey {
        target: OwnedItem {
            owner,
            item: ItemId("typed-login".into()),
        },
        action: None,
    };
    let binding = executor
        .root_binding(&key, None)
        .expect("complete catalog grants typed binding");
    let receipt = executor
        .submit_typed(&binding, typed_values("conformance-secret"))
        .unwrap();
    assert!(!receipt.receipt.query_history_allowed);
    loop {
        if let Some(completion) = executor.poll_plugin_actions().into_iter().next() {
            assert_eq!(completion.request_id, receipt.receipt.request_id);
            assert!(matches!(
                completion.outcome,
                Ok(crikey_app::ActionEffect::Typed(SubmissionOutcome::Completed))
            ));
            break;
        }
        assert!(Instant::now() < deadline, "typed execution did not finish");
        sleep(Duration::from_millis(2));
    }
}

#[test]
fn real_native_typed_reacquisition_refuses_changed_physical_worker_schema() {
    use crikey_core::structured_arguments::{
        ExecutableKey, OwnedItem, SubmissionFailureCode, SubmissionOutcome,
    };
    let scratch = Scratch::new("typed-schema-change");
    let (binary, _) = conformance_binaries();
    let package = write_native_plugin(&scratch.path, "typed-change", &binary, "typed");
    let owner = native_plugin("typed-change");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load(
        &mut pipeline,
        &[scratch.path.clone()],
        &DisabledPlugins::default(),
    );
    provider
        .request_catalog_build(&owner, 1, Generation::ZERO)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(result) = provider.take_catalog_results().into_iter().next() {
            assert!(
                matches!(result, crikey_app::CatalogBuildResult::Complete(_)),
                "{result:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "typed catalog did not finish");
        sleep(Duration::from_millis(2));
    }
    fs::write(package.join("conformance-mode"), "typed-changed").unwrap();
    let driver = NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
    let executor = driver.action_executor();
    let key = ExecutableKey {
        target: OwnedItem {
            owner,
            item: ItemId("typed-login".into()),
        },
        action: None,
    };
    let binding = executor.root_binding(&key, None).unwrap();
    executor
        .submit_typed(&binding, typed_values("conformance-secret"))
        .unwrap();
    loop {
        if let Some(completion) = executor.poll_plugin_actions().into_iter().next() {
            assert!(matches!(
                completion.outcome,
                Ok(crikey_app::ActionEffect::Typed(SubmissionOutcome::Failed(
                    SubmissionFailureCode::SchemaMismatch
                )))
            ));
            break;
        }
        assert!(Instant::now() < deadline, "typed schema refusal did not finish");
        sleep(Duration::from_millis(2));
    }
}

#[test]
fn real_native_typed_suggestions_retire_execution_but_retain_the_selected_scope() {
    use crikey_core::result_presentation::ScopeId;
    use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
    let scratch = Scratch::new("typed-source");
    let (binary, _) = conformance_binaries();
    write_native_plugin(&scratch.path, "typed-source", &binary, "typed");
    let owner = native_plugin("typed-source");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load(
        &mut pipeline,
        &[scratch.path.clone()],
        &DisabledPlugins::default(),
    );
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut held = false;
    let publication_gate = Mutex::new(move |view: &ViewModel| {
        if view.generation == Generation::from_raw(1) && !held {
            held = true;
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(ROW_DELIVERY_WINDOW);
        }
    });
    let driver = NativeDriver::spawn(
        provider,
        pipeline,
        Box::new(move |view| publication_gate.lock().unwrap()(view)),
    );
    let executor = driver.action_executor();
    let key = ExecutableKey {
        target: OwnedItem {
            owner: owner.clone(),
            item: ItemId("typed-suggestion".into()),
        },
        action: None,
    };
    driver.submit(Generation::from_raw(1), "typed", 0, Vec::new(), false, 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    let binding = loop {
        if let Some(binding) = executor.root_binding(&key, None) {
            break binding;
        }
        assert!(
            Instant::now() < deadline,
            "typed suggestion publication did not arrive"
        );
        sleep(Duration::from_millis(2));
    };
    let scope = crikey_app::RootScope {
        owner,
        id: ScopeId("secure".into()),
        source: binding.source.clone(),
    };
    assert!(executor.root_scope_current(&scope));
    let mut missing = scope.clone();
    missing.id = ScopeId("missing".into());
    assert!(!executor.root_scope_current(&missing));
    let mut replacement = scope.clone();
    if let crikey_app::RootSource::Suggestion { producer, .. } = &mut replacement.source {
        *producer = producer.checked_add(1).unwrap();
    } else {
        panic!("scope must originate only from a dynamic suggestion");
    }
    assert!(!executor.root_scope_current(&replacement));
    let mut foreign = scope.clone();
    foreign.owner = native_plugin("not-registered");
    assert!(!executor.root_scope_current(&foreign));
    entered_rx
        .recv_timeout(ROW_DELIVERY_WINDOW)
        .expect("first publication reaches the held provider thread");
    driver
        .submit_scoped(
            ProviderQueryContext {
                generation: Generation::from_raw(2),
                text: "typed",
                now: 1,
                selected: 0,
            },
            Vec::new(),
            false,
            Some(scope.clone()),
        )
        .unwrap();
    assert!(
        executor.root_scope_current(&scope),
        "selected dynamic scope survives executable-generation retirement"
    );
    assert!(executor
        .submit_typed(&binding, typed_values("conformance-secret"))
        .is_err());
    assert!(
        executor.root_binding(&key, None).is_none(),
        "no executable survives while the scoped query is still pending"
    );
    release_tx.send(()).unwrap();
    let current = loop {
        if let Some(current) = executor.root_binding(&key, None) {
            assert_ne!(current.source, binding.source);
            break current;
        }
        assert!(
            Instant::now() < deadline,
            "scoped typed suggestion did not arrive"
        );
        sleep(Duration::from_millis(2));
    };
    assert!(executor.root_scope_current(&scope));
    let receipt = executor
        .submit_typed(&current, typed_values("conformance-secret"))
        .expect("fresh scoped publication grants executable authority");
    await_condition(ROW_DELIVERY_WINDOW, "fresh scoped typed execution", || {
        executor.poll_plugin_actions().into_iter().any(|completion| {
            completion.request_id == receipt.receipt.request_id
                && matches!(
                    completion.outcome,
                    Ok(crikey_app::ActionEffect::Typed(
                        crikey_core::structured_arguments::SubmissionOutcome::Completed
                    ))
                )
        })
    });
    driver
        .submit_scoped(
            ProviderQueryContext {
                generation: Generation::from_raw(3),
                text: "typed next",
                now: 2,
                selected: 0,
            },
            Vec::new(),
            false,
            Some(scope.clone()),
        )
        .expect("the original captured token remains valid across typed queries");
    assert!(executor.root_scope_current(&scope));
    await_condition(ROW_DELIVERY_WINDOW, "next scoped publication", || {
        executor
            .root_binding(&key, None)
            .is_some_and(|next| next.source != current.source)
    });
    driver.submit(Generation::from_raw(4), "typed", 3, Vec::new(), false, 0);
    assert!(!executor.root_scope_current(&scope));
    assert!(
        driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(5),
                    text: "typed",
                    now: 4,
                    selected: 0
                },
                Vec::new(),
                false,
                Some(scope.clone())
            )
            .is_err(),
        "All revokes capture; an old uncaptured declaration cannot silently become All"
    );
    let latest = loop {
        if let Some(latest) = executor.root_binding(&key, None) {
            break latest;
        }
        assert!(Instant::now() < deadline, "All suggestion did not arrive");
        sleep(Duration::from_millis(2));
    };
    let latest_scope = crikey_app::RootScope {
        source: latest.source,
        ..scope
    };
    driver
        .submit_scoped(
            ProviderQueryContext {
                generation: Generation::from_raw(5),
                text: "typed",
                now: 4,
                selected: 0,
            },
            Vec::new(),
            false,
            Some(latest_scope.clone()),
        )
        .unwrap();
    assert!(executor.root_scope_current(&latest_scope));
    drop(driver);
    assert!(
        !executor.root_scope_current(&latest_scope),
        "producer shutdown revokes even a captured token"
    );
}

fn wait_native_action(
    executor: &dyn crikey_app::PluginActionExecutor,
    expected: &crikey_app::ActionRequestId,
) -> crikey_core::Result<crikey_app::ActionEffect> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(completion) = executor.poll_plugin_actions().into_iter().next() {
            assert_eq!(&completion.request_id, expected);
            return completion.outcome;
        }
        assert!(Instant::now() < deadline, "foreground callback did not terminate");
        sleep(Duration::from_millis(2));
    }
}

#[test]
fn real_native_standard_routes_revoke_exact_revision_and_producer_without_root_fallback() {
    use crikey_app::ActionEffect;
    use crikey_core::structured_arguments::*;
    use crikey_core::structured_view::*;
    use std::num::NonZeroU64;
    let scratch = Scratch::new("standard-routes");
    let (ordinary_binary, _) = conformance_binaries();
    let binary = ordinary_binary.parent().unwrap().join(format!(
        "structured-view-fixture{}",
        if cfg!(windows) { ".exe" } else { "" }
    ));
    assert!(
        binary.is_file(),
        "standard fixture must be built with the real SDK"
    );
    write_native_manifest(&scratch.path, "views", Some(&binary), None, None);
    let owner = native_plugin("views");
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = NativeProvider::load(
        &mut pipeline,
        &[scratch.path.clone()],
        &DisabledPlugins::default(),
    );
    assert!(provider.unavailable().is_empty(), "{:?}", provider.unavailable());
    provider
        .request_catalog_build(&owner, 1, Generation::ZERO)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(result) = provider.take_catalog_results().into_iter().next() {
            assert!(
                matches!(result, crikey_app::CatalogBuildResult::Complete(_)),
                "{result:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline);
        sleep(Duration::from_millis(2));
    }
    let driver = NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
    let executor = driver.action_executor();
    let launch = executor
        .submit_plugin_action_by_id(&owner, &ItemId("launch".into()), &ActionId("open".into()), None)
        .unwrap();
    let producer = match wait_native_action(executor.as_ref(), &launch).unwrap() {
        ActionEffect::Navigate {
            outcome: StructuredRouteOutcome::Push(route),
            producer,
        } => {
            assert_eq!(route.0, "grid");
            producer
        }
        effect => panic!("root callback lost navigation: {effect:?}"),
    };
    let request = |route: &str, surface: u64| ViewRequest {
        mode: ViewRequestMode::Open,
        route: RouteId(route.into()),
        session: ViewSessionIdentity {
            logical_view: NonZeroU64::new(surface).unwrap(),
            surface: NonZeroU64::new(surface).unwrap(),
        },
        previous_revision: None,
        state: None,
        query: String::new(),
        scroll: None,
        reason: ViewRefreshReason::Explicit,
    };
    let open = |request: ViewRequest| {
        let id = executor.request_view(&owner, producer, request).unwrap();
        match wait_native_action(executor.as_ref(), &id).unwrap() {
            ActionEffect::View(reply) => reply,
            effect => panic!("not a descriptor: {effect:?}"),
        }
    };
    let key = |action: Option<&str>| ExecutableKey {
        target: OwnedItem {
            owner: owner.clone(),
            item: ItemId("shared-item".into()),
        },
        action: action.map(|action| ActionId(action.into())),
    };
    let host_state = |reply: &ViewReply| HostViewState {
        selected: Some(key(None).target),
        scope: None,
        input: match &reply.descriptor.view {
            StandardView::Form(form) => Some(form.submit.clone()),
            _ => None,
        },
    };
    let grid = open(request("grid", 1));
    assert!(matches!(grid.descriptor.view, StandardView::Grid(_)));
    let mut refresh = request("grid", 1);
    refresh.mode = ViewRequestMode::Refresh;
    refresh.previous_revision = Some(grid.context.revision);
    refresh.state = Some(host_state(&grid));
    refresh.query = "coalesce".into();
    struct ReleaseCoalescingCallback(PathBuf);
    impl Drop for ReleaseCoalescingCallback {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, b"release");
        }
    }
    let release = ReleaseCoalescingCallback(scratch.path.join("views/route-coalesce-release"));
    let pending_refresh = executor.request_view(&owner, producer, refresh).unwrap();
    let running = scratch.path.join("views/route-coalesce-running");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !running.is_file() {
        assert!(
            Instant::now() < deadline,
            "delayed Refresh never entered the actual worker"
        );
        sleep(Duration::from_millis(2));
    }
    // New host intent changes only the existing P3 query/state, not the
    // outstanding callback. Only the latest intent is dispatched after terminal.
    let mut latest_query = String::new();
    for intent in ["coalesced-a", "coalesced-b", "coalesced-latest"] {
        latest_query.clear();
        latest_query.push_str(intent);
    }
    fs::write(&release.0, b"release").unwrap();
    let refreshed = match wait_native_action(executor.as_ref(), &pending_refresh).unwrap() {
        ActionEffect::View(reply) => reply,
        effect => panic!("uncancelled Refresh lost its actual revision: {effect:?}"),
    };
    assert_eq!(refreshed.context.session, grid.context.session);
    let previous = refreshed.context.revision;
    let mut latest = request("grid", 1);
    latest.mode = ViewRequestMode::Refresh;
    latest.previous_revision = Some(previous);
    latest.state = Some(host_state(&refreshed));
    latest.scroll = Some(ScrollAnchor {
        item: key(None).target,
        row_offset: 7,
    });
    latest.query = latest_query;
    latest.reason = ViewRefreshReason::Query;
    let refreshed = open(latest);
    assert_eq!(refreshed.context.session, grid.context.session);
    assert_ne!(refreshed.context.revision, previous);
    assert!(refreshed.descriptor.title.contains("coalesced-latest"));
    assert!(!refreshed.descriptor.title.contains("coalesced-a"));
    fs::remove_file(running).unwrap();
    let mut removal = request("grid", 1);
    removal.mode = ViewRequestMode::Refresh;
    removal.previous_revision = Some(refreshed.context.revision);
    removal.state = Some(host_state(&refreshed));
    removal.scroll = Some(ScrollAnchor {
        item: key(None).target,
        row_offset: 7,
    });
    removal.query = "remove-selected".into();
    removal.reason = ViewRefreshReason::Query;
    let removed = open(removal);
    assert_eq!(removed.context.session, refreshed.context.session);
    assert!(
        removed.descriptor.definitions.is_empty(),
        "filter replacement must actually remove the selected executable"
    );
    assert!(
        executor
            .submit_view_action(
                &owner,
                &refreshed.context,
                &host_state(&refreshed),
                &key(None),
                None
            )
            .is_err(),
        "old revision cannot execute a removed target"
    );
    assert!(
        executor
            .submit_view_action(
                &owner,
                &removed.context,
                &host_state(&refreshed),
                &key(None),
                None
            )
            .is_err(),
        "restore intent never authorizes a target absent from the current descriptor"
    );
    let mut restore = request("grid", 1);
    restore.mode = ViewRequestMode::Refresh;
    restore.previous_revision = Some(removed.context.revision);
    restore.state = Some(host_state(&refreshed));
    restore.scroll = Some(ScrollAnchor {
        item: key(None).target,
        row_offset: 7,
    });
    restore.query = "coalesced-latest".into();
    restore.reason = ViewRefreshReason::Query;
    let refreshed = open(restore);
    assert_ne!(refreshed.context.revision, removed.context.revision);
    assert_eq!(refreshed.context.session, removed.context.session);
    assert!(
        executor
            .submit_view_action(
                &owner,
                &removed.context,
                &host_state(&refreshed),
                &key(None),
                None
            )
            .is_err(),
        "removed-descriptor events cannot borrow the restored definitions"
    );
    let mut unselected = host_state(&refreshed);
    unselected.selected = None;
    assert!(
        executor
            .submit_view_action(&owner, &refreshed.context, &unselected, &key(None), None)
            .is_err(),
        "descriptor membership is not selected-event authority"
    );
    assert_ne!(grid.context.revision, refreshed.context.revision);
    assert!(!executor.view_current(&owner, &grid.context));
    assert!(
        executor.view_current(&owner, &refreshed.context),
        "stale validation must not retire the replacement"
    );
    assert!(executor
        .submit_view_action(&owner, &grid.context, &host_state(&grid), &key(None), None)
        .is_err());
    assert!(
        executor
            .submit_view_action(
                &owner,
                &refreshed.context,
                &host_state(&refreshed),
                &key(Some("open")),
                None
            )
            .is_err(),
        "colliding root action must not resolve in actionless grid"
    );
    let id = executor
        .submit_view_action(
            &owner,
            &refreshed.context,
            &host_state(&refreshed),
            &key(None),
            None,
        )
        .unwrap();
    assert!(
        matches!(wait_native_action(executor.as_ref(), &id).unwrap(), ActionEffect::Navigate { outcome: StructuredRouteOutcome::Push(RouteId(route)), producer: actual } if route == "detail" && actual == producer)
    );
    let detail = open(request("detail", 2));
    executor
        .validate_view_link(
            &owner,
            &detail.context,
            &host_state(&detail),
            "https://example.com/",
        )
        .unwrap();
    assert!(executor
        .validate_view_link(
            &owner,
            &detail.context,
            &host_state(&detail),
            "https://example.com/not-admitted"
        )
        .is_err());
    let id = executor
        .submit_view_action(
            &owner,
            &detail.context,
            &host_state(&detail),
            &key(Some("open")),
            None,
        )
        .unwrap();
    assert!(
        matches!(wait_native_action(executor.as_ref(), &id).unwrap(), ActionEffect::Navigate { outcome: StructuredRouteOutcome::Replace(RouteId(route)), .. } if route == "list")
    );
    let list = open(request("list", 3));
    let id = executor
        .submit_view_action(
            &owner,
            &list.context,
            &host_state(&list),
            &key(Some("open")),
            None,
        )
        .unwrap();
    assert!(
        matches!(wait_native_action(executor.as_ref(), &id).unwrap(), ActionEffect::Navigate { outcome: StructuredRouteOutcome::Push(RouteId(route)), .. } if route == "form")
    );
    let form = open(request("form", 4));
    let mut unfocused = host_state(&form);
    unfocused.input = None;
    assert!(
        executor
            .submit_view_action(&owner, &form.context, &unfocused, &key(Some("open")), None)
            .is_err(),
        "typed route targets cannot bypass their input schema"
    );
    let values = |password: &str, destination: &str| {
        ArgumentValues(std::collections::BTreeMap::from([
            (
                FieldId("username".into()),
                StructuredValue::NonSecret(NonSecretValue::Text("operator".into())),
            ),
            (
                FieldId("password".into()),
                StructuredValue::Password(Password::new(password.to_owned()).unwrap()),
            ),
            (
                FieldId("destination".into()),
                StructuredValue::NonSecret(NonSecretValue::Dropdown(OptionId(destination.into()))),
            ),
        ]))
    };
    assert!(
        executor
            .submit_view_typed(
                &owner,
                &form.context,
                &unfocused,
                &key(Some("open")),
                values("view-secret", "complete")
            )
            .is_err(),
        "typed submission requires the current P3 input target"
    );
    let id = executor
        .submit_view_typed(
            &owner,
            &form.context,
            &host_state(&form),
            &key(Some("open")),
            values("wrong", "complete"),
        )
        .unwrap();
    assert!(matches!(
        wait_native_action(executor.as_ref(), &id).unwrap(),
        ActionEffect::Typed(SubmissionOutcome::InvalidFields(_))
    ));
    let id = executor
        .submit_view_typed(
            &owner,
            &form.context,
            &host_state(&form),
            &key(Some("open")),
            values("view-secret", "complete"),
        )
        .unwrap();
    assert!(matches!(
        wait_native_action(executor.as_ref(), &id).unwrap(),
        ActionEffect::Typed(SubmissionOutcome::Completed)
    ));
    let id = executor
        .submit_view_typed(
            &owner,
            &form.context,
            &host_state(&form),
            &key(Some("open")),
            values("view-secret", "navigate"),
        )
        .unwrap();
    assert!(
        matches!(wait_native_action(executor.as_ref(), &id).unwrap(), ActionEffect::Navigate { outcome: StructuredRouteOutcome::Replace(RouteId(route)), producer: actual } if route == "grid" && actual == producer)
    );
    executor.retire_view(&owner, form.context.session);
    assert!(!executor.view_current(&owner, &form.context));
    assert!(executor
        .submit_view_typed(
            &owner,
            &form.context,
            &host_state(&form),
            &key(Some("open")),
            values("view-secret", "complete")
        )
        .is_err());
    // Back opens the route directly with a fresh surface; no action is replayed.
    let restored = open(request("list", 5));
    assert_eq!(restored.context.route.0, "list");
    executor.retire_view(&owner, restored.context.session);
    let mut delayed = request("grid", 6);
    delayed.query = "delay".into();
    let id = executor.request_view(&owner, producer, delayed).unwrap();
    let running = scratch.path.join("views").join("route-running");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !running.is_file() {
        assert!(
            Instant::now() < deadline,
            "the real structured callback never entered"
        );
        sleep(Duration::from_millis(2));
    }
    assert!(executor.cancel_plugin_action(&id));
    assert!(wait_native_action(executor.as_ref(), &id).is_err());
    // No intervening view/preview operation may reset the physical latch.
    // The same live worker must accept a new Root call after cooperative cancel.
    let root = executor
        .submit_plugin_action_by_id(&owner, &ItemId("launch".into()), &ActionId("open".into()), None)
        .unwrap();
    assert!(
        matches!(wait_native_action(executor.as_ref(), &root).unwrap(), ActionEffect::Navigate { producer: actual, .. } if actual == producer)
    );
    let mut death = request("grid", 7);
    death.query = "source-death".into();
    let id = executor.request_view(&owner, producer, death).unwrap();
    assert!(wait_native_action(executor.as_ref(), &id).is_err());
    assert!(
        executor
            .request_view(&owner, producer, request("grid", 8))
            .is_err(),
        "dead route producer must never be reacquired"
    );
}
