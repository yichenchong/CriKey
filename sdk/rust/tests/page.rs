//! Red-first tests for plugin-drawn pages across the SDK boundary (spec 27.3,
//! 27.4, 16.7).

use crikey_core::{NodeRole, PageFrame, PageInput, PageInputKind, Result};
use crikey_native_protocol::{Capabilities, Endpoint};
use crikey_plugin_sdk::{
    harness::TestHarness, CatalogSink, ExecuteOutcome, ExecuteRequest, PageBuilder, PageRect, PageRequest,
    Plugin, PluginContext, Query, ServeConfig, SuggestionSink,
};

const PAGE_ID: &str = "counter";

fn config() -> ServeConfig {
    ServeConfig {
        plugin_id: "page.test".to_owned(),
        plugin_name: "Page Test Plugin".to_owned(),
        plugin_version: "1.0.0".to_owned(),
        sdk_version: "sdk-test".to_owned(),
        capabilities: Capabilities {
            streaming_catalog: true,
            streaming_suggestions: true,
            cancellation: true,
            configuration_updates: false,
            events: false,
        },
        endpoint: Some(Endpoint::Stdio),
        session_token: Some("page-session".to_owned()),
    }
}

/// How a fixture answers a page request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageBehaviour {
    /// Draws a real frame and counts the events it was handed.
    Draw,
    /// Builds its frame on a generation of its own invention, which the SDK
    /// must overwrite with the one being answered.
    StaleGeneration,
    /// Panics, which the serving loop must survive.
    Panic,
}

struct PagePlugin {
    behaviour: PageBehaviour,
    opens_page: bool,
    seen_events: usize,
}

impl PagePlugin {
    fn new(behaviour: PageBehaviour) -> Self {
        Self {
            behaviour,
            opens_page: true,
            seen_events: 0,
        }
    }

    fn completing_execute() -> Self {
        Self {
            behaviour: PageBehaviour::Draw,
            opens_page: false,
            seen_events: 0,
        }
    }
}

impl Plugin for PagePlugin {
    fn start(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    fn build_catalog(&mut self, _context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.emit_batch(Vec::new())?;
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

    fn execute_outcome(
        &mut self,
        request: ExecuteRequest,
        context: &dyn PluginContext,
    ) -> Result<ExecuteOutcome> {
        if self.opens_page {
            return Ok(ExecuteOutcome::show_page(PAGE_ID));
        }
        self.execute(request, context).map(ExecuteOutcome::from)
    }

    fn page(&mut self, request: PageRequest, _context: &dyn PluginContext) -> Result<PageFrame> {
        match self.behaviour {
            PageBehaviour::Panic => panic!("page fixture panicked"),
            PageBehaviour::Draw | PageBehaviour::StaleGeneration => {
                let generation = match self.behaviour {
                    PageBehaviour::StaleGeneration => 0,
                    _ => request.generation,
                };
                self.seen_events = self.seen_events.saturating_add(request.events.len());
                Ok(PageBuilder::new(generation, request.palette)
                    .title(format!("{} ({} events)", request.page_id, self.seen_events))
                    .heading(24.0, 24.0, "Counter")
                    .button(1, PageRect::new(24.0, 72.0, 120.0, 32.0), "Increment")
                    .checkbox(2, 24.0, 120.0, "Loud", request.focused)
                    .focus(1)
                    .build())
            }
        }
    }
}

/// A plugin that never overrides `page`, so the trait default answers.
struct DefaultPagePlugin;

impl Plugin for DefaultPagePlugin {
    fn start(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    fn build_catalog(&mut self, _context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.emit_batch(Vec::new())?;
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

fn opened() -> Vec<PageInput> {
    vec![PageInput::new(PageInputKind::Opened)]
}

#[test]
fn page_request_is_answered_with_a_frame_echoing_the_generation() {
    let mut harness = TestHarness::start(PagePlugin::new(PageBehaviour::Draw), config()).expect("harness");
    let frame = harness.page(PAGE_ID, 7, opened(), true).expect("page frame");
    assert_eq!(
        frame.generation, 7,
        "the frame must answer the generation it was asked for"
    );
    assert_eq!(frame.title, "counter (1 events)");
    assert!(!frame.close);
    assert_eq!(frame.focus_ring(), vec![1, 2]);
    assert_eq!(
        frame.node(1).map(|node| node.role),
        Some(NodeRole::Button),
        "the button must carry its role, not only its rectangle"
    );
    assert_eq!(
        frame.node(1).and_then(|node| node.accessible_name()),
        Some("Increment")
    );
    assert!(frame.unlabelled_interactive().is_empty());
    frame.validate().expect("a builder frame is drawable");

    // A second request on the same page must be answered on its own
    // generation: a plugin that echoed the first would have its later frames
    // dropped by the host.
    let next = harness
        .page(PAGE_ID, 8, Vec::new(), true)
        .expect("second page frame");
    assert_eq!(next.generation, 8);
    harness.shutdown().expect("shutdown");
}

#[test]
fn a_plugin_that_returns_a_stale_generation_still_answers_the_request() {
    // The SDK stamps the requested generation over whatever the plugin put
    // there, because a page that silently stops repainting is far harder to
    // diagnose than one that never drew.
    let mut harness =
        TestHarness::start(PagePlugin::new(PageBehaviour::StaleGeneration), config()).expect("harness");
    let frame = harness.page(PAGE_ID, 3, opened(), false).expect("page frame");
    assert_eq!(frame.generation, 3);
    assert_eq!(
        frame.node(2).map(|node| node.checked),
        Some(false),
        "an unfocused page must see focused=false"
    );
    harness.shutdown().expect("shutdown");
}

#[test]
fn a_panicking_page_callback_closes_the_page_and_keeps_the_connection() {
    let mut harness = TestHarness::start(PagePlugin::new(PageBehaviour::Panic), config()).expect("harness");
    let frame = harness.page(PAGE_ID, 2, opened(), true).expect("page frame");
    assert!(frame.close, "a failed page must close rather than linger");
    assert_eq!(frame.generation, 2);
    assert!(frame.nodes.is_empty());
    // The connection survives: a later request on it is still served.
    let again = harness
        .page(PAGE_ID, 3, Vec::new(), true)
        .expect("second page frame");
    assert!(again.close);
    harness.shutdown().expect("shutdown");
}

#[test]
fn the_default_page_implementation_closes_the_page() {
    let mut harness = TestHarness::start(DefaultPagePlugin, config()).expect("harness");
    let frame = harness.page("unowned", 5, opened(), true).expect("page frame");
    assert!(
        frame.close,
        "a plugin that cannot draw must not leave the surface open"
    );
    assert_eq!(frame.generation, 5);
    harness.shutdown().expect("shutdown");
}

#[test]
fn execute_can_report_that_it_opened_a_page() {
    let mut harness = TestHarness::start(PagePlugin::new(PageBehaviour::Draw), config()).expect("harness");
    let outcome = harness
        .execute_outcome("item", Some("open"), None)
        .expect("execute outcome");
    assert_eq!(outcome, ExecuteOutcome::show_page(PAGE_ID));
    harness.shutdown().expect("shutdown");
}

#[test]
fn an_execute_returning_unit_still_reports_completion() {
    let mut harness = TestHarness::start(PagePlugin::completing_execute(), config()).expect("harness");
    assert_eq!(
        harness
            .execute_outcome("item", None, None)
            .expect("execute outcome"),
        ExecuteOutcome::Completed
    );
    harness.execute("item", None, None).expect("execute");
    harness.shutdown().expect("shutdown");
}
