//! Real native preview worker. Build with:
//! cargo build -p crikey-plugin-sdk --example preview
//! Launch target/debug/examples/preview as a native plugin executable. The host
//! supplies CRIKEY_PLUGIN_ENDPOINT/CRIKEY_SESSION_TOKEN/CRIKEY_PLUGIN_ID.
use crikey_core::{CoreError, Result};
use crikey_plugin_sdk::{
    serve, ActionBuilder, CatalogSink, ExecuteRequest, ItemBuilder, Plugin, PluginContext, PreviewContent,
    PreviewImage, PreviewOutcome, PreviewText, PreviewValue, PreviewValueKind, Query, ServeConfig,
    SuggestionSink,
};

struct PreviewDemo {
    enabled: bool,
    prepared: bool,
    witnesses: Option<std::path::PathBuf>,
}
fn items() -> Vec<crikey_core::Item> {
    ["text", "markdown", "long", "image", "slow", "empty", "error"]
        .into_iter()
        .map(|id| {
            ItemBuilder::new(id, format!("Preview {id}"))
                .preview_deferred()
                .action(ActionBuilder::new("acknowledge", "Acknowledge").build())
                .build()
        })
        .collect()
}

impl Plugin for PreviewDemo {
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> {
        // Test consumers declare an exact scratch directory in this read-only
        // package file; the installed package itself is never writable.
        self.witnesses = match std::fs::read_to_string("preview-witness-directory") {
            Ok(directory) => Some(directory.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(CoreError::Invalid(error.to_string())),
        };
        Ok(())
    }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        self.prepared = true;
        sink.emit_batch(items())?;
        sink.finish()
    }
    fn suggest(&mut self, query: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        if query.text == "catalog-only" {
            return sink.finish();
        }
        self.prepared = true;
        sink.emit_batch(items())?;
        sink.finish()
    }
    fn execute(&mut self, request: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        if let Some(witnesses) = &self.witnesses {
            use std::io::Write as _;
            if !witnesses.join("preview-terminal").exists() {
                return Err(CoreError::Invalid("action overlapped preview callback".into()));
            }
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(witnesses.join("executions"))
                .map_err(|error| CoreError::Invalid(error.to_string()))?;
            writeln!(
                log,
                "{}:{}:{}",
                std::process::id(),
                request.item.0,
                request.action.as_ref().map_or("none", |action| action.0.as_str())
            )
            .map_err(|error| CoreError::Invalid(error.to_string()))?;
            let hold = witnesses.join("action-hold");
            let release = witnesses.join("action-release");
            while hold.exists() && !release.exists() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Ok(())
    }
    fn supports_preview(&self) -> bool {
        self.enabled
    }
    fn preview(&mut self, item_id: &str, context: &dyn PluginContext) -> Result<PreviewOutcome> {
        if !self.prepared {
            return Err(CoreError::Invalid(
                "selected item belongs to an unprepared worker".into(),
            ));
        }
        if item_id == "empty" {
            return Ok(PreviewOutcome::Empty);
        }
        if item_id == "error" {
            return Err(CoreError::Invalid("demonstration preview failure".into()));
        }
        if item_id == "slow" {
            if let Some(witnesses) = &self.witnesses {
                let pid = std::process::id().to_string();
                let record = |name| {
                    std::fs::write(witnesses.join(name), &pid)
                        .map_err(|error| CoreError::Invalid(error.to_string()))
                };
                record("preview-started")?;
                let mut observed_cancel = false;
                let release = witnesses.join("preview-release");
                while !release.exists() {
                    if context.cancellation().is_cancelled() && !observed_cancel {
                        record("preview-cancelled")?;
                        observed_cancel = true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                record("preview-terminal")?;
                // Deliberately return content even after cancellation. The
                // consumer must reject it without inventing an early terminal.
                return Ok(PreviewOutcome::Content(PreviewContent {
                    text: Some(PreviewText::Plain("late speculative content".into())),
                    ..Default::default()
                }));
            }
            // This real protocol record synchronizes the conformance consumer:
            // it may cancel only after this callback has actually begun.
            context.log(crikey_plugin_sdk::LogLevel::Debug, "preview callback started");
            loop {
                if context.cancellation().is_cancelled() {
                    return Ok(PreviewOutcome::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let text = match item_id {
            "markdown" => PreviewText::Markdown(
                "# Native preview\n\n**Bounded markdown** without fetched assets.".into(),
            ),
            "long" => {
                use std::fmt::Write as _;
                let mut source = String::with_capacity(16 * 1024);
                source.push_str("# Scrollable native preview\n\n");
                for line in 1..=150 {
                    writeln!(source, "- **Line {line}**: bounded worker-produced content for independent preview scrolling.")
                        .expect("writing to a String cannot fail");
                }
                PreviewText::Markdown(source)
            }
            "text" | "slow" | "image" => PreviewText::Plain(format!(
                "Selected {item_id}; worker process {}",
                std::process::id()
            )),
            _ => return Ok(PreviewOutcome::Unsupported),
        };
        Ok(PreviewOutcome::Content(PreviewContent {
            text: Some(text),
            image: (item_id == "image").then(|| PreviewImage {
                width: 2,
                height: 2,
                rgba: vec![
                    255, 96, 32, 255, 32, 192, 96, 255, 32, 96, 255, 255, 240, 240, 240, 255,
                ],
            }),
            values: vec![
                PreviewValue {
                    label: "Runtime".into(),
                    value: "native Rust".into(),
                    kind: PreviewValueKind::Tag,
                },
                PreviewValue {
                    label: "Selected item".into(),
                    value: item_id.into(),
                    kind: PreviewValueKind::Text,
                },
                PreviewValue {
                    label: "Documentation".into(),
                    value: "https://github.com/yichenchong/CriKey".into(),
                    kind: PreviewValueKind::Link,
                },
            ],
        }))
    }
}

fn main() {
    let enabled = !std::env::args().any(|arg| arg == "--no-preview");
    if let Err(error) = serve(
        &mut PreviewDemo {
            enabled,
            prepared: false,
            witnesses: None,
        },
        ServeConfig::new("preview-demo", "0.1.0"),
    ) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
