//! Private-desktop P3 fixture; logs lifecycle identities, never query/argument text.
use crikey_core::{CoreError, PageFrame, PageInputKind, Result};
use crikey_plugin_sdk::{
    serve, ActionBuilder, CatalogSink, ExecuteOutcome, ExecuteRequest, ItemBuilder, PageBuilder, PageRequest,
    Plugin, PluginContext, Query, ServeConfig, SuggestionSink,
};
use std::io::Write;

fn log_path() -> Result<std::path::PathBuf> {
    std::env::current_exe()
        .map(|path| path.with_file_name("navigation-lifecycle.log"))
        .map_err(|error| CoreError::Invalid(format!("navigation fixture log path: {error}")))
}
fn record(message: &str) -> Result<()> {
    let path = log_path()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| {
            CoreError::Invalid(format!(
                "navigation fixture cannot open {}: {error}",
                path.display()
            ))
        })?;
    if file
        .metadata()
        .map_err(|error| CoreError::Invalid(error.to_string()))?
        .len()
        >= 1024 * 1024
    {
        return Err(CoreError::Invalid(
            "navigation fixture log reached 1 MiB; archive/clear the private fixture log".into(),
        ));
    }
    writeln!(file, "{message}")
        .and_then(|_| file.sync_data())
        .map_err(|error| {
            CoreError::Invalid(format!(
                "navigation fixture cannot write {}: {error}",
                path.display()
            ))
        })
}
fn opened_count() -> Result<usize> {
    let path = log_path()?;
    let file = std::fs::File::open(&path).map_err(|error| CoreError::Invalid(error.to_string()))?;
    if file
        .metadata()
        .map_err(|error| CoreError::Invalid(error.to_string()))?
        .len()
        >= 1024 * 1024
    {
        return Err(CoreError::Invalid("navigation fixture log reached 1 MiB".into()));
    }
    use std::io::BufRead;
    std::io::BufReader::new(file).lines().try_fold(0, |count, line| {
        line.map(|line| count + usize::from(line.starts_with("opened ")))
            .map_err(|error| CoreError::Invalid(error.to_string()))
    })
}
fn items() -> Vec<crikey_core::Item> {
    (0..40)
        .map(|index| {
            ItemBuilder::new(format!("route-{index}"), format!("Navigation fixture {index:02}"))
                .action(ActionBuilder::new("open", "Open custom route").build())
                .build()
        })
        .collect()
}
struct NavigationFixture {
    opened: usize,
}
impl Plugin for NavigationFixture {
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> {
        record("worker started")
    }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.emit_batch(items())?;
        sink.finish()
    }
    fn suggest(&mut self, _: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        sink.finish()
    }
    fn execute(&mut self, _: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn execute_outcome(&mut self, request: ExecuteRequest, _: &dyn PluginContext) -> Result<ExecuteOutcome> {
        record(&format!("execute {}", request.item.0))?;
        Ok(ExecuteOutcome::show_page(request.item.0))
    }
    fn page(&mut self, request: PageRequest, _: &dyn PluginContext) -> Result<PageFrame> {
        for event in &request.events {
            if event.kind == PageInputKind::Opened {
                // Read the fixture-only log afresh: a reopened route may use
                // a replacement worker, not this process's previous counter.
                self.opened = opened_count()? + 1;
                record(&format!(
                    "opened {} {} {}",
                    request.page_id, request.generation, self.opened
                ))?;
            }
            if event.kind == PageInputKind::Closed {
                record(&format!("closed {} {}", request.page_id, request.generation))?;
            }
        }
        Ok(PageBuilder::new(request.generation, request.palette)
            .title(format!("{} — opening {}", request.page_id, self.opened))
            .heading(
                24.0,
                24.0,
                format!("{} — opening {}", request.page_id, self.opened),
            )
            .heading(24.0, 72.0, "Open host Settings, then Back to reopen this route")
            .build())
    }
}
fn main() {
    if let Err(error) = serve(
        &mut NavigationFixture { opened: 0 },
        ServeConfig::new("navigation-fixture", "0.1.0"),
    ) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
