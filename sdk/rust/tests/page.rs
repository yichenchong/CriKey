//! Red-first tests for plugin-drawn pages across the SDK boundary (spec 32.3,
//! 32.7, 16.7).

use crikey_core::{NodeRole, NodeShape, PageColor, PageError, PageFrame, PageInput, PageInputKind, Result};
use crikey_native_protocol::{Capabilities, Endpoint};
use crikey_plugin_sdk::{
    harness::TestHarness, CatalogSink, ExecuteOutcome, ExecuteRequest, PageBuilder, PagePalette, PageRect,
    PageRequest, Plugin, PluginContext, Query, ServeConfig, SuggestionSink,
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
                    .image(
                        PageRect::new(200.0, 72.0, 32.0, 32.0),
                        "Swatch",
                        2,
                        2,
                        vec![0x40; 2 * 2 * 4],
                    )
                    .expect("the swatch matches its own dimensions")
                    // Drawn from the event count so the raster path is proven
                    // live rather than blitted once and cached.
                    .canvas(PageRect::new(200.0, 120.0, 64.0, 16.0), "", 8, 4, |canvas| {
                        let bar = self.seen_events.min(canvas.width() as usize) as u32;
                        canvas.fill_rect(0, 0, bar, canvas.height(), PageColor::rgba(0, 0, 0, 255));
                    })
                    .expect("the bar matches its own dimensions")
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

/// Stands in for the colours the host hands a page when a test builds a
/// frame without going through a request.
fn palette() -> PagePalette {
    PagePalette {
        surface: PageColor::rgba(0x20, 0x20, 0x20, 0xFF),
        text: PageColor::rgba(0xF0, 0xF0, 0xF0, 0xFF),
        accent: PageColor::rgba(0x30, 0x80, 0xF0, 0xFF),
        muted: PageColor::rgba(0x80, 0x80, 0x80, 0xFF),
    }
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

/// The rasters the fixture draws must survive the SDK boundary, and the
/// painted one must follow the plugin's state: a page whose canvas is
/// computed once and cached would still pass every other assertion here.
#[test]
fn rasters_cross_the_boundary_and_the_painted_one_tracks_plugin_state() {
    let mut harness = TestHarness::start(PagePlugin::new(PageBehaviour::Draw), config()).expect("harness");
    let frame = harness.page(PAGE_ID, 1, opened(), true).expect("page frame");
    frame.validate().expect("a frame carrying rasters is drawable");
    let rasters: Vec<_> = frame
        .nodes
        .iter()
        .filter(|node| node.shape == NodeShape::Image)
        .collect();
    assert_eq!(rasters.len(), 2, "both rasters must reach the host");
    let swatch = rasters[0].image.as_ref().expect("the swatch carries pixels");
    assert_eq!(
        (swatch.pixel_width, swatch.pixel_height, swatch.rgba.len()),
        (2, 2, 16)
    );
    assert_eq!(
        rasters[0].accessible_name(),
        Some("Swatch"),
        "a labelled raster must announce the name the author gave it"
    );
    assert_eq!(
        rasters[1].accessible_name(),
        None,
        "an unlabelled raster is decoration, not a nameless announcement"
    );
    let first_bar = rasters[1].image.clone().expect("the bar carries pixels");
    assert_eq!(first_bar.rgba[0..4], [0, 0, 0, 255], "one event, one column");
    assert_eq!(first_bar.rgba[4..8], [0, 0, 0, 0]);

    let next = harness
        .page(PAGE_ID, 2, opened(), true)
        .expect("second page frame");
    let second_bar = next
        .nodes
        .iter()
        .filter(|node| node.shape == NodeShape::Image)
        .nth(1)
        .and_then(|node| node.image.clone())
        .expect("the bar carries pixels");
    assert_ne!(
        second_bar.rgba, first_bar.rgba,
        "the painted raster must be repainted from current state, not cached"
    );
    assert_eq!(second_bar.rgba[4..8], [0, 0, 0, 255], "two events, two columns");
    harness.shutdown().expect("shutdown");
}

/// The canvas helper owns the stride arithmetic, so what it writes has to
/// land where the author asked in the raster's own coordinates.
#[test]
fn the_canvas_helper_addresses_pixels_not_bytes() {
    let frame = PageBuilder::new(1, palette())
        .canvas(PageRect::new(0.0, 0.0, 8.0, 8.0), "Chart", 3, 2, |canvas| {
            canvas.fill(PageColor::rgba(1, 2, 3, 4));
            canvas.set_pixel(2, 1, PageColor::rgba(9, 8, 7, 6));
            // Off the edge: clipped, because a bar computed from plugin state
            // must not be able to take the worker down.
            canvas.set_pixel(3, 1, PageColor::rgba(0, 0, 0, 255));
            canvas.fill_rect(2, 0, 10, 1, PageColor::rgba(5, 5, 5, 5));
        })
        .expect("a 3x2 canvas is a valid raster")
        .build();
    frame.validate().expect("a painted frame is drawable");
    let image = frame.nodes[0].image.as_ref().expect("pixels");
    assert_eq!(image.rgba.len(), 3 * 2 * 4);
    assert_eq!(image.rgba[8..12], [5, 5, 5, 5], "row 0, column 2");
    assert_eq!(image.rgba[20..24], [9, 8, 7, 6], "row 1, column 2");
    assert_eq!(image.rgba[12..16], [1, 2, 3, 4], "row 1, column 0 keeps the fill");
}

/// A raster that cannot exist is refused where it was written, not where it
/// is drawn: the host would drop the whole frame, and the author would be
/// looking at a blank page for a byte count they could have been told about.
#[test]
fn an_impossible_raster_is_refused_at_construction() {
    let builder = || PageBuilder::new(1, palette()).heading(0.0, 0.0, "Page");
    assert_eq!(
        builder()
            .image(PageRect::new(0.0, 0.0, 8.0, 8.0), "", 4, 4, vec![0; 60])
            .err(),
        Some(PageError::ImageSizeMismatch {
            index: 1,
            expected: 64,
            actual: 60
        })
    );
    assert_eq!(
        builder()
            .image(PageRect::new(0.0, 0.0, 8.0, 8.0), "", 0, 4, Vec::new())
            .err(),
        Some(PageError::ImageEdgeOutOfRange {
            index: 1,
            pixel_width: 0,
            pixel_height: 4
        })
    );
    assert_eq!(
        builder()
            .canvas(PageRect::new(0.0, 0.0, 8.0, 8.0), "", 2048, 2, |_| {
                unreachable!("a refused canvas must never be painted")
            })
            .err(),
        Some(PageError::ImageEdgeOutOfRange {
            index: 1,
            pixel_width: 2048,
            pixel_height: 2
        })
    );
    assert_eq!(
        builder()
            .canvas(PageRect::new(0.0, 0.0, 8.0, 8.0), "", 1024, 1024, |_| {
                unreachable!("a canvas over the per-node cap must never be painted")
            })
            .err(),
        Some(PageError::ImageTooLarge {
            index: 1,
            bytes: 1024 * 1024 * 4
        })
    );
}
