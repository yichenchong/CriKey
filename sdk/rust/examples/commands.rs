//! Executable commands_v1 example. Build: cargo build -p crikey-plugin-sdk --example commands
//! The host builds this worker's catalog before requesting declarations, then
//! binds its installed owner after validating the current catalog targets.
//! For the reusable GUI fixture, put `integration` in package-cwd
//! `command-ui-mode` (an explicit argv mode wins). That mode declares shared-a,
//! shared-b, alternate, actionless, required and forbidden. Successful callbacks
//! append exact item/action/argument Debug fields to command-ui-executions.log,
//! capped at 1 MiB. Sentinel queries: command-ui-conflict, command-ui-fail and
//! command-ui-pending (five seconds, cooperative cancellation).
//! Integration ui-shared has a real deferred COMMAND_UI_CATALOG preview and
//! https://example.invalid/command-fixture; command-ui-previews.log records its
//! producing PID under the same 1 MiB witness cap. The conflicting suggestion
//! instead has inline COMMAND_UI_SUGGESTION_INLINE_NO_LINK and no link authority:
//! the native preview callback receives only an item ID, not source provenance.
use crikey_core::{
    CoreError, PreviewContent, PreviewOutcome, PreviewText, PreviewValue, PreviewValueKind, Result,
};
use crikey_plugin_sdk::{
    serve, ActionBuilder, CatalogSink, CommandBuilder, ExecuteRequest, ItemBuilder, Plugin, PluginCommands,
    PluginContext, Query, ServeConfig, SuggestionSink,
};
use std::io::Write;

const EXECUTION_LOG: &str = "command-ui-executions.log";
const EXECUTION_LOG_LIMIT: u64 = 1024 * 1024;
const PREVIEW_LOG: &str = "command-ui-previews.log";
const CANONICAL_PREVIEW: &str = "COMMAND_UI_CATALOG";
const CANONICAL_LINK: &str = "https://example.invalid/command-fixture";

fn integration_catalog() -> Vec<crikey_core::Item> {
    let mut shared = ItemBuilder::new("ui-shared", "Shared catalog item")
        .target("canonical shared")
        .action(ActionBuilder::new("primary", "Primary").build())
        .action(ActionBuilder::new("alternate", "Alternate").build())
        .preview_deferred()
        .build();
    shared.argument_policy = crikey_core::ArgumentPolicy::Optional;
    shared.search_terms.push("command-ui-conflict".into());
    let mut actionless = ItemBuilder::new("ui-actionless", "Actionless catalog item").build();
    actionless.argument_policy = crikey_core::ArgumentPolicy::Optional;
    let mut required = ItemBuilder::new("ui-required", "Required catalog item")
        .action(ActionBuilder::new("primary", "Primary").build())
        .build();
    required.argument_policy = crikey_core::ArgumentPolicy::Required;
    let forbidden = ItemBuilder::new("ui-forbidden", "Forbidden catalog item")
        .action(ActionBuilder::new("primary", "Primary").build())
        .build();
    vec![shared, actionless, required, forbidden]
}

fn integration_commands() -> Result<PluginCommands> {
    use crikey_core::ArgumentPolicy::{Forbidden, Optional, Required};
    Ok(PluginCommands::Declarations(vec![
        CommandBuilder::new("shared-a", "Shared command", "ui-shared")
            .action("primary")
            .arguments(Optional)
            .build()?,
        CommandBuilder::new("shared-b", "Shared command", "ui-shared")
            .action("primary")
            .arguments(Optional)
            .build()?,
        CommandBuilder::new("alternate", "Alternate command", "ui-shared")
            .action("alternate")
            .arguments(Optional)
            .build()?,
        CommandBuilder::new("actionless", "Actionless command", "ui-actionless")
            .arguments(Optional)
            .build()?,
        CommandBuilder::new("required", "Required command", "ui-required")
            .action("primary")
            .arguments(Required)
            .build()?,
        CommandBuilder::new("forbidden", "Forbidden command", "ui-forbidden")
            .action("primary")
            .arguments(Forbidden)
            .build()?,
    ]))
}

fn record_integration_execution(request: &ExecuteRequest) -> Result<()> {
    let action = request.action.as_ref().map(|action| action.0.as_str());
    let valid = match request.item.0.as_str() {
        "ui-shared" => matches!(action, Some("primary" | "alternate" | "suggestion-action")),
        "ui-actionless" => action.is_none(),
        "ui-required" => {
            action == Some("primary")
                && request
                    .argument
                    .as_deref()
                    .is_some_and(|argument| !argument.is_empty())
        }
        "ui-forbidden" => action == Some("primary") && request.argument.is_none(),
        _ => false,
    };
    if !valid {
        return Err(CoreError::Invalid(
            "unknown integration command target or argument policy".into(),
        ));
    }
    let line = format!(
        "item={:?}\taction={:?}\targument={:?}\n",
        request.item.0, action, request.argument
    );
    append_witness(EXECUTION_LOG, &line)
}

fn append_witness(path: &str, line: &str) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| CoreError::Invalid(error.to_string()))?;
    let size = file
        .metadata()
        .map_err(|error| CoreError::Invalid(error.to_string()))?
        .len();
    if size.saturating_add(line.len() as u64) > EXECUTION_LOG_LIMIT {
        return Err(CoreError::Invalid("command execution log reached 1 MiB".into()));
    }
    file.write_all(line.as_bytes())
        .and_then(|_| file.sync_data())
        .map_err(|error| CoreError::Invalid(error.to_string()))
}

struct CommandsDemo {
    mode: String,
    catalog_ready: bool,
}
impl Plugin for CommandsDemo {
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        if self.mode == "integration" {
            sink.emit_batch(integration_catalog())?;
            self.catalog_ready = true;
            return sink.finish();
        }
        sink.emit_batch(vec![ItemBuilder::new("report", "Report")
            .action(ActionBuilder::new("open", "Open report").build())
            .build()])?;
        self.catalog_ready = true;
        sink.finish()
    }
    fn suggest(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        if self.mode == "integration" {
            match query.text.as_str() {
                "command-ui-conflict" => {
                    sink.emit_batch(vec![ItemBuilder::new("ui-shared", "Conflicting live suggestion")
                        .target("suggestion, not canonical catalog")
                        .description("COMMAND_UI_SUGGESTION_INLINE_NO_LINK")
                        .action(ActionBuilder::new("suggestion-action", "Suggestion action").build())
                        .build()])?
                }
                "command-ui-fail" => return Err(CoreError::Invalid("integration suggestion failure".into())),
                "command-ui-pending" => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while std::time::Instant::now() < deadline && !context.cancellation().is_cancelled() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
                _ => {}
            }
        }
        sink.finish()
    }
    fn execute(&mut self, request: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        if self.mode == "integration" {
            return record_integration_execution(&request);
        }
        if request.item.0 != "report" || request.action.as_ref().is_some_and(|action| action.0 != "open") {
            return Err(CoreError::Invalid("unknown command target".into()));
        }
        Ok(())
    }
    fn supports_preview(&self) -> bool {
        self.mode == "integration"
    }

    fn preview(&mut self, item_id: &str, context: &dyn PluginContext) -> Result<PreviewOutcome> {
        if self.mode != "integration" {
            return Ok(PreviewOutcome::Unsupported);
        }
        if context.cancellation().is_cancelled() {
            return Ok(PreviewOutcome::Cancelled);
        }
        if !self.catalog_ready || item_id != "ui-shared" {
            return Err(CoreError::Invalid(
                "preview requires this catalog's producing worker".into(),
            ));
        }
        append_witness(
            PREVIEW_LOG,
            &format!(
                "item={item_id:?}\tmarker={CANONICAL_PREVIEW:?}\tlink={CANONICAL_LINK:?}\tpid={}\n",
                std::process::id(),
            ),
        )?;
        Ok(PreviewOutcome::Content(PreviewContent {
            text: Some(PreviewText::Plain(CANONICAL_PREVIEW.into())),
            image: None,
            values: vec![PreviewValue {
                label: "Documentation".into(),
                value: CANONICAL_LINK.into(),
                kind: PreviewValueKind::Link,
            }],
        }))
    }

    fn supports_commands(&self) -> bool {
        self.mode != "unsupported"
    }
    fn commands(&mut self, context: &dyn PluginContext) -> Result<PluginCommands> {
        if !self.catalog_ready {
            return Err(CoreError::Invalid("catalog must initialize this worker".into()));
        }
        if self.mode == "integration" {
            return integration_commands();
        }
        if self.mode == "slow" {
            context.log(crikey_plugin_sdk::LogLevel::Debug, "commands callback started");
            while !context.cancellation().is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            return Ok(PluginCommands::Cancelled);
        }
        let mut declaration = CommandBuilder::new("open-report", "Open report", "report")
            .action("open")
            .build()?;
        match self.mode.as_str() {
            "invalid" => declaration.command_id.clear(),
            "oversized" => declaration.label = "x".repeat(8193),
            "bad-target" => declaration.item_id.0 = "not-in-catalog".into(),
            "aggregate" => {
                return Ok(PluginCommands::Declarations(
                    (0..600)
                        .map(|index| {
                            let mut command = declaration.clone();
                            command.command_id = format!("command-{index}");
                            command.label = "x".repeat(8192);
                            command
                        })
                        .collect(),
                ))
            }
            "count" => return Ok(PluginCommands::Declarations(vec![declaration; 10_001])),
            _ => {}
        }
        Ok(PluginCommands::Declarations(vec![declaration]))
    }
}
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| {
        if std::fs::read_to_string("command-ui-mode").is_ok_and(|mode| mode.trim() == "integration") {
            "integration".into()
        } else {
            "valid".into()
        }
    });
    if let Err(error) = serve(
        &mut CommandsDemo {
            mode,
            catalog_ready: false,
        },
        ServeConfig::new("commands-demo", "0.1.0"),
    ) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
