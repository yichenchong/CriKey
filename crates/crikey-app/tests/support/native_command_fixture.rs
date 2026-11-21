struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-native-commands-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// A dedicated real SDK plugin is compiled only when this integration gate runs.
// The callback insists on catalog-local state and stamps the process identity,
// so a duplicate declaration child cannot accidentally satisfy these scenarios.
fn executable() -> &'static Path {
    static BINARY: LazyLock<(Scratch, PathBuf)> = LazyLock::new(|| {
        let scratch = Scratch::new();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        fs::create_dir_all(scratch.0.join("src")).unwrap();
        fs::write(
            scratch.0.join("Cargo.toml"),
            format!(
                r#"[package]
name = "native-command-intake-fixture"
version = "0.0.0"
edition = "2021"
[workspace]
[dependencies]
crikey-plugin-sdk = {{ path = {:?} }}
crikey-core = {{ path = {:?} }}
"#,
                root.join("sdk/rust"),
                root.join("crates/crikey-core")
            ),
        )
        .unwrap();
        fs::write(scratch.0.join("src/main.rs"), FIXTURE).unwrap();
        let target = root.join("target/native-command-intake");
        let output = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(scratch.0.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let binary = target.join("debug").join(if cfg!(windows) {
            "native-command-intake-fixture.exe"
        } else {
            "native-command-intake-fixture"
        });
        (scratch, binary)
    });
    &BINARY.1
}

const FIXTURE: &str = r#"
use crikey_core::{CoreError, Result, PluginId};
use crikey_plugin_sdk::{serve, ActionBuilder, CatalogSink, CommandBuilder, ExecuteRequest, ItemBuilder, Plugin, PluginCommands, PluginContext, Query, ServeConfig, SuggestionSink};
struct Worker { mode: String, prepared: bool }
impl Plugin for Worker {
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> { Ok(()) }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> { Ok(()) }
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        self.prepared = true;
        let mut item = ItemBuilder::new("installed", std::process::id().to_string())
            .action(if self.mode == "host-action" {
                ActionBuilder::new("alternate", "Alternate").host_mediated().build()
            } else { ActionBuilder::new("alternate", "Alternate").build() }).build();
        if self.mode == "repeated-duplicate" {
            item.actions.push(item.actions[0].clone());
        }
        if self.mode == "repeated-unrelated-duplicate" {
            let unrelated = ActionBuilder::new("unrelated", "Unrelated").build();
            item.actions.extend([unrelated.clone(), unrelated]);
        }
        if self.mode == "primary-host" {
            item.actions[0].execution_policy = crikey_core::ExecutionPolicy::HostMediated;
        }
        if self.mode == "primary-category" {
            item.actions[0].applicable_categories =
                vec![crikey_core::Category::PluginDefined("other-category".into())];
        }
        if self.mode == "primary-duplicate" {
            item.actions.push(item.actions[0].clone());
        }
        if self.mode == "primary-plugin-host-alternate" {
            item.actions.push(ActionBuilder::new("host-alternate", "Host alternate").host_mediated().build());
        }
        if self.mode == "primary-actionless" {
            item.actions.clear();
        }
        if self.mode == "handoff" {
            item.metadata.insert("crikey.preview".into(), "deferred".into());
            item.argument_policy = crikey_core::ArgumentPolicy::Required;
        }
        // Even a catalog item's claimed namespace is not command owner authority.
        item.plugin_id = PluginId("crikey.host".into());
        sink.emit_batch(vec![item])?;
        sink.finish()
    }
    fn supports_commands(&self) -> bool { self.mode != "unsupported" }
    fn commands(&mut self, _: &dyn PluginContext) -> Result<PluginCommands> {
        if !self.prepared { return Err(CoreError::Invalid("commands ran in a second child".into())); }
        if self.mode == "gated" {
            while !std::path::Path::new("release").exists() { std::thread::sleep(std::time::Duration::from_millis(2)); }
        }
        if self.mode == "deadline" { std::thread::sleep(std::time::Duration::from_secs(2)); }
        if self.mode == "error" { return Err(CoreError::Invalid("declaration callback refused".into())); }
        if self.mode == "cancelled" { return Ok(PluginCommands::Cancelled); }
        if self.mode.starts_with("repeated") {
            let commands = (0..128).map(|index| {
                let mut command = CommandBuilder::new(
                    format!("repeat-{index}"), format!("Repeated {index}"), "installed"
                ).build()?;
                command.action_id = Some(crikey_core::ActionId("alternate".into()));
                Ok(command)
            }).collect::<Result<Vec<_>>>()?;
            return Ok(PluginCommands::Declarations(commands));
        }
        let item = if self.mode == "bad-target" { "suggestion-only" } else { "installed" };
        let mut command = CommandBuilder::new("crikey.host", std::process::id().to_string(), item).build()?;
        if self.mode == "bad-action" { command.action_id = Some(crikey_core::ActionId("absent".into())); }
        if self.mode == "action" || self.mode == "host-action" { command.action_id = Some(crikey_core::ActionId("alternate".into())); }
        if self.mode == "bad-arguments" { command.argument_policy = crikey_core::ArgumentPolicy::Required; }
        if self.mode == "handoff" {
            command.argument_policy = crikey_core::ArgumentPolicy::Required;
            command.action_id = Some(crikey_core::ActionId("alternate".into()));
        }
        Ok(PluginCommands::Declarations(vec![command]))
    }
    fn suggest(&mut self, _: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        sink.emit_batch(vec![ItemBuilder::new("suggestion-only", std::process::id().to_string()).build()])?;
        sink.finish()
    }
    fn supports_preview(&self) -> bool { self.mode == "handoff" }
    fn preview(&mut self, _: &str, context: &dyn PluginContext) -> Result<crikey_plugin_sdk::PreviewOutcome> {
        std::fs::write("preview-started", "").unwrap();
        while !context.cancellation().is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::fs::write("preview-cancelled", "").unwrap();
        while !std::path::Path::new("preview-release").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(crikey_plugin_sdk::PreviewOutcome::Cancelled)
    }
    fn execute(&mut self, request: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        let action = request.action.as_ref().map_or("<default>", |id| id.0.as_str());
        if self.mode == "handoff" {
            use std::io::Write;
            let mut log = std::fs::OpenOptions::new().create(true).append(true).open("executions").unwrap();
            writeln!(log, "{}:{action}:{}", request.item.0, request.argument.as_deref().unwrap_or("")).unwrap();
            return Ok(());
        }
        std::fs::write("executed", format!("{}:{action}", request.item.0))
            .map_err(|error| CoreError::Invalid(error.to_string()))
    }
}
fn main() {
    let mode = std::fs::read_to_string("mode").unwrap();
    // Block before serve's handshake, so the host is still acquiring its lazy
    // worker, rather than already having dispatched Execute to a ready link.
    if std::path::Path::new("startup-block").exists() {
        std::fs::write("startup-entered", "").unwrap();
        while !std::path::Path::new("startup-release").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    serve(&mut Worker { mode, prepared: false }, ServeConfig::new("commands", "0.1.0")).unwrap();
}
"#;

fn wait_for<T>(mut observe: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = observe() {
            return value;
        }
        assert!(
            Instant::now() < end,
            "supervised declaration operation did not terminate"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn setup(mode: &str) -> (Scratch, QueryPipeline, NativeProvider, PluginId) {
    let scratch = Scratch::new();
    let package = scratch.0.join("commands");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("mode"), mode).unwrap();
    fs::write(
        package.join("crikey.toml"),
        format!(
            r#"manifest-version = 1
[plugin]
id = "commands"
name = "Commands"
version = "0.1.0"
runtime = "native"
entrypoint = {:?}
[performance]
startup = "lazy"
suggest-hard-timeout-ms = 250
[concurrency]
max-catalog-tasks = 2
"#,
            executable()
        ),
    )
    .unwrap();
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider = NativeProvider::load_with_collection_window(
        &mut pipeline,
        &[scratch.0.clone()],
        Duration::from_secs(5),
        &DisabledPlugins::default(),
    );
    let owner = PluginId("native.commands".into());
    assert_eq!(
        provider.plugins(),
        &[owner.clone()],
        "{:?}",
        provider.unavailable()
    );
    (scratch, pipeline, provider, owner)
}

fn complete(provider: &mut NativeProvider) -> CatalogBuildResult {
    wait_for(|| provider.take_catalog_results().into_iter().next())
}
fn snapshot(provider: &NativeProvider, owner: &PluginId) -> Arc<NativeCommandSnapshot> {
    provider
        .command_snapshot(owner)
        .expect("terminal catalog work publishes declaration status")
}
fn assert_catalog(result: CatalogBuildResult) -> String {
    let CatalogBuildResult::Complete(build) = result else {
        panic!("ordinary catalog was discarded: {result:?}");
    };
    assert_eq!(build.items.len(), 1);
    build.items[0].label.clone()
}
