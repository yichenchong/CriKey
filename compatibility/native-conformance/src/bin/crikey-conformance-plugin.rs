//! Third-party-shaped native plugin used by the M5 conformance acceptance test.
//!
//! This binary intentionally depends only on the published Rust SDK and core
//! model. It is an out-of-tree process: the host launches it and communicates
//! over the endpoint named by the host-set environment (spec 16.1, 16.6,
//! acceptance §31.21, §31.22).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crikey_core::{CoreError, PageColor, PageFrame, PageInput, PageInputKind, Result};
use crikey_plugin_sdk::{
    serve, ActionBuilder, CatalogSink, ExecuteOutcome, ExecuteRequest, ItemBuilder, PageBuilder,
    PageCanvas, PagePalette, PageRect, PageRequest, Plugin, PluginContext, PluginResource, Query,
    ResourceKind, SdkError, ServeConfig, SuggestionSink,
};

const MODE_ENV: &str = "CRIKEY_CONFORMANCE_MODE";
const MODE_FILE: &str = "conformance-mode";
const PLUGIN_ID: &str = "conformance";
const PLUGIN_NAME: &str = "CriKey Native Conformance";
const PLUGIN_VERSION: &str = "1.0.0";
const SHARED_PLUGIN_ID: &str = "shared.identity";

#[derive(Debug, Clone)]
enum Mode {
    Echo,
    SameId,
    EnvWitness,
    Stream(usize),
    Acceptance,
    Slow(u64),
    SlowWitness(u64),
    IgnoreCancel(u64),
    CrashOnSuggest,
    CrashOnStart,
    FailSuggest,
    Sequence,
    Icon,
    IconSlow,
    IconSilent,
    /// Publishes the interactive demo page and nothing else, so the surface a
    /// host draws is attributable to exactly one item.
    Page,
}

fn candidate(value: Option<String>) -> Option<String> {
    value
        .map(|mode| mode.trim().to_owned())
        .filter(|mode| !mode.is_empty())
}

fn selected_mode() -> String {
    let from_file = || {
        let path: PathBuf = env::current_dir().ok()?.join(MODE_FILE);
        fs::read_to_string(path).ok()
    };

    candidate(env::var(MODE_ENV).ok())
        .or_else(|| candidate(env::args().nth(1)))
        .or_else(|| candidate(from_file()))
        .unwrap_or_else(|| "echo".to_owned())
}
fn parse_mode(spec: &str) -> Mode {
    match spec {
        "echo" => Mode::Echo,
        "same-id" => Mode::SameId,
        "env-witness" => Mode::EnvWitness,
        "acceptance" => Mode::Acceptance,
        "crash-on-suggest" => Mode::CrashOnSuggest,
        "crash-on-start" => Mode::CrashOnStart,
        "fail-suggest" => Mode::FailSuggest,
        "sequence" => Mode::Sequence,
        "icon" => Mode::Icon,
        "icon-slow" => Mode::IconSlow,
        "icon-silent" => Mode::IconSilent,
        "page" => Mode::Page,
        _ if spec.starts_with("stream:") => Mode::Stream(
            spec.strip_prefix("stream:")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0),
        ),
        _ if spec.starts_with("slow-witness:") => Mode::SlowWitness(
            spec.strip_prefix("slow-witness:")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        ),
        _ if spec.starts_with("slow:") => Mode::Slow(
            spec.strip_prefix("slow:")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        ),
        _ if spec.starts_with("ignore-cancel:") => Mode::IgnoreCancel(
            spec.strip_prefix("ignore-cancel:")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        ),
        _ => Mode::Echo,
    }
}

fn sequence_stage() -> Mode {
    let path = env::var_os("CRIKEY_SEQUENCE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join(format!("crikey-sequence-{}", std::process::id())));
    let count = fs::read_to_string(&path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let _ = fs::write(&path, count.saturating_add(1).to_string());
    match count {
        0 | 2 => Mode::CrashOnSuggest,
        _ => Mode::Echo,
    }
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::Invalid(message.into())
}

fn pid_target() -> String {
    std::process::id().to_string()
}

fn item(
    stable_id: impl Into<String>,
    label: impl Into<String>,
    target: impl Into<String>,
) -> crikey_core::Item {
    ItemBuilder::new(stable_id, label).target(target).build()
}

fn catalog_items() -> Vec<crikey_core::Item> {
    vec![
        pid_item(),
        item("catalog-1", "Conformance one", "conformance://one"),
        item("catalog-2", "Conformance two", "conformance://two"),
    ]
}

fn catalog_stream_items(count: usize) -> Vec<crikey_core::Item> {
    (0..count)
        .map(|index| {
            item(
                format!("stream-catalog-{index}"),
                format!("Conformance catalog #{index}"),
                format!("conformance://catalog-{index}"),
            )
        })
        .collect()
}

fn environment_items() -> Vec<crikey_core::Item> {
    let mut variables: Vec<(String, String)> = env::vars().collect();
    variables.sort_by(|left, right| left.0.cmp(&right.0));
    variables
        .into_iter()
        .map(|(name, value)| item(format!("env:{name}"), name.clone(), value))
        .collect()
}

/// Suggestion results carry a DETERMINISTIC target: `crikey dev
/// inspect-protocol` prints item targets and its output must be byte-for-byte
/// reproducible across runs, so a volatile value like the process id can never
/// live here. The pid is reported by `pid_item` instead, which only the
/// out-of-process proof (§31.30) asks for.
fn result_item(stable_id: impl Into<String>, label: impl Into<String>) -> crikey_core::Item {
    let stable_id = stable_id.into();
    let target = format!("conformance://{stable_id}");
    item(stable_id, label, target)
}

/// The one item whose target is deliberately volatile: this process's id, so a
/// caller can prove the plugin ran outside the host process (§31.30).
fn pid_item() -> crikey_core::Item {
    item("pid", "conformance process", pid_target())
}

/// The reference that resolves to a real icon.
const ICON_SERVED: &str = "served.svg";
/// The reference the fixture has no icon for.
const ICON_MISSING: &str = "absent.svg";
/// The reference whose icon is valid but larger than any host will accept.
const ICON_OVERSIZED: &str = "oversized.svg";
/// The reference the fixture answers correctly, but only after the host has
/// already stopped waiting.
const ICON_LATE: &str = "late.svg";

/// How long [`ICON_LATE`] withholds its answer.
///
/// Comfortably past any host icon deadline and comfortably inside the
/// suggestion deadline, so this fixture proves the host gives up on a slow
/// resource without also proving it kills a plugin that is merely busy.
const ICON_LATE_DELAY: Duration = Duration::from_millis(400);

/// The reference an `icon-slow` plugin answers, but only once the host's
/// result collection window has closed.
const ICON_AFTER_WINDOW: &str = "after-window.svg";
/// The reference an `icon-silent` plugin never answers.
const ICON_NEVER: &str = "never.svg";

/// How long [`ICON_AFTER_WINDOW`] withholds its answer.
///
/// Deliberately between the host's 100 ms result collection window and its
/// 250 ms icon deadline. The icon therefore cannot reach the batch that named
/// it and must still arrive afterwards, which is what makes "results first,
/// picture later" observable at all. A host that widened its collection window
/// past this value would collapse the two events into one and this fixture
/// would stop distinguishing them.
const ANSWERS_AFTER_THE_COLLECTION_WINDOW: Duration = Duration::from_millis(150);

/// How long [`ICON_NEVER`] withholds its answer.
///
/// Far past the host's 250 ms icon deadline and past any plausible growth of
/// it, so the host must abandon the request rather than ever receive a reply.
/// The fixture sleeps rather than returning nothing on purpose: declining is a
/// prompt answer, and this case is about a plugin that gives none.
const NEVER_ANSWERS: Duration = Duration::from_secs(10);

/// A real, decodable icon, written as SVG so the fixture needs no image codec.
fn icon_bytes(padding: usize) -> Vec<u8> {
    let mut svg = String::with_capacity(padding + 256);
    svg.push_str(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\" viewBox=\"0 0 16 16\">",
    );
    if padding > 0 {
        // Padding lives in a comment *after* the root element opens: the host
        // sniffs `<svg` inside a bounded prologue, so a document padded at the
        // front would be rejected as "not an icon" instead of "too large",
        // which is a different test.
        svg.push_str("<!--");
        svg.push_str(&"p".repeat(padding));
        svg.push_str("-->");
    }
    svg.push_str("<rect width=\"16\" height=\"16\" fill=\"#3366cc\"/></svg>");
    svg.into_bytes()
}

/// One item naming one icon reference, so a host's icon behaviour is
/// attributable to exactly one row.
fn icon_item(stable_id: &str, reference: &str) -> crikey_core::Item {
    ItemBuilder::new(stable_id, stable_id)
        .target(format!("conformance://{stable_id}"))
        .icon(reference)
        .build()
}

/// One item per host-observable icon outcome, so a single query exercises the
/// whole matrix against the real process boundary.
fn icon_items() -> Vec<crikey_core::Item> {
    [
        ("icon-served", ICON_SERVED),
        ("icon-missing", ICON_MISSING),
        ("icon-oversized", ICON_OVERSIZED),
        ("icon-late", ICON_LATE),
    ]
    .into_iter()
    .map(|(stable_id, reference)| icon_item(stable_id, reference))
    .collect()
}

/// The page this fixture draws. One plugin may own several, so the identifier
/// travels in every request rather than being assumed.
const PAGE_ID: &str = "playground";
/// The item whose action opens the page.
const PAGE_ITEM_ID: &str = "page-playground";
/// The action that opens it. Named separately from the item because the host
/// may run it as the item's default action or as a chosen one.
const PAGE_ACTION_ID: &str = "open-page";

/// Node identities, stable across every frame this fixture draws: the host
/// reports the node an event landed on, so these are the fixture's entire
/// input routing table.
const NODE_DECREASE: u32 = 1;
const NODE_INCREASE: u32 = 2;
const NODE_ANNOUNCE: u32 = 3;
const NODE_NOTE: u32 = 4;
const NODE_CLOSE: u32 = 5;

/// Longest note the page keeps. A page rebuilds its display list on every
/// keystroke, so the text it carries has to be bounded by the plugin: the host
/// bounds the frame, not the plugin's own state.
const MAX_NOTE_CHARS: usize = 48;

/// Side of the decorative swatch, in pixels.
const SWATCH_EDGE: u32 = 4;

/// The swatch as finished pixels. A plugin shipping a PNG would have decoded
/// it into exactly this before building its frame: the host takes raw RGBA8
/// and parses no image format.
const SWATCH_RGBA: [u8; (SWATCH_EDGE * SWATCH_EDGE * 4) as usize] = [
    0x30, 0x80, 0xF0, 0xFF, 0xE8, 0xAE, 0x58, 0xFF, 0x30, 0x80, 0xF0, 0xFF, 0xE8, 0xAE, 0x58, 0xFF,
    0xE8, 0xAE, 0x58, 0xFF, 0x30, 0x80, 0xF0, 0xFF, 0xE8, 0xAE, 0x58, 0xFF, 0x30, 0x80, 0xF0, 0xFF,
    0x30, 0x80, 0xF0, 0xFF, 0xE8, 0xAE, 0x58, 0xFF, 0x30, 0x80, 0xF0, 0xFF, 0xE8, 0xAE, 0x58, 0xFF,
    0xE8, 0xAE, 0x58, 0xFF, 0x30, 0x80, 0xF0, 0xFF, 0xE8, 0xAE, 0x58, 0xFF, 0x30, 0x80, 0xF0, 0xFF,
];

/// Columns the counter chart can show, and therefore the counter value at
/// which the chart is full.
const CHART_COLUMNS: u32 = 12;

/// Width of one chart column and the gap after it, in raster pixels.
const CHART_COLUMN_WIDTH: u32 = 4;
const CHART_COLUMN_GAP: u32 = 1;

/// The chart's own resolution. Small on purpose: the raster is scaled into the
/// node's rectangle, so a chart does not need one pixel per logical pixel.
const CHART_PIXEL_WIDTH: u32 = CHART_COLUMNS * (CHART_COLUMN_WIDTH + CHART_COLUMN_GAP);
const CHART_PIXEL_HEIGHT: u32 = 24;

/// The item a user searches for to reach the page.
fn page_item() -> crikey_core::Item {
    ItemBuilder::new(PAGE_ITEM_ID, "Page Playground")
        .target(format!("page:{PAGE_ID}"))
        .description("Open the plugin-drawn demo page")
        .search_term("page")
        .search_term("playground")
        .action(
            ActionBuilder::new(PAGE_ACTION_ID, "Open Page")
                .description("Draw the conformance plugin's interactive page")
                .build(),
        )
        .build()
}

/// Everything the demo page remembers between frames.
///
/// The page is redrawn from scratch on every request, so this is the only
/// place its content exists: a host that dropped a frame or asked twice for
/// the same generation cannot change what the user sees.
#[derive(Debug, Default)]
struct PageState {
    counter: i64,
    announce: bool,
    note: String,
    /// The node the host last told the page was focused, which is how typed
    /// text knows where to land. The host owns focus appearance; the page only
    /// owns what focus means.
    focused_node: u32,
    /// The node this page will ask the host to focus on its next frame, and
    /// only that frame: repeating the request every frame would drag focus
    /// back out of wherever the user moved it.
    pending_focus: u32,
    /// Set when the user asked the page to finish, so the next frame closes it
    /// from the plugin's side instead of waiting for Escape.
    closing: bool,
}

impl PageState {
    /// Applies one host event. Unknown kinds are ignored rather than guessed
    /// at: an SDK newer than this fixture may route events it never heard of.
    fn apply(&mut self, event: &PageInput) {
        match event.kind {
            PageInputKind::Opened => {
                *self = Self {
                    focused_node: NODE_NOTE,
                    pending_focus: NODE_NOTE,
                    ..Self::default()
                };
            }
            PageInputKind::FocusChanged => self.focused_node = event.node_id,
            PageInputKind::Activated => match event.node_id {
                NODE_DECREASE => self.counter = self.counter.saturating_sub(1),
                NODE_INCREASE => self.counter = self.counter.saturating_add(1),
                NODE_ANNOUNCE => self.announce = !self.announce,
                NODE_CLOSE => self.closing = true,
                _ => {}
            },
            PageInputKind::KeyPressed => match event.key.as_str() {
                "ArrowUp" => self.counter = self.counter.saturating_add(1),
                "ArrowDown" => self.counter = self.counter.saturating_sub(1),
                "Backspace" if self.focused_node == NODE_NOTE => {
                    self.note.pop();
                }
                _ => {}
            },
            PageInputKind::TextInput if self.focused_node == NODE_NOTE => {
                for character in event.text.chars() {
                    if self.note.chars().count() >= MAX_NOTE_CHARS {
                        break;
                    }
                    self.note.push(character);
                }
            }
            _ => {}
        }
    }

    /// Draws the page: a heading, a counter the user changes, a checkbox, a
    /// note the user types into, and a button that ends the page.
    fn frame(&mut self, request: &PageRequest) -> PageFrame {
        let palette = request.palette;
        let width = request.width as f32;
        let height = request.height as f32;
        let mut page = PageBuilder::new(request.generation, palette)
            .title("Page Playground")
            .rect(0.0, 0.0, width, height, palette.surface)
            .heading(28.0, 24.0, "Page Playground")
            .text(
                28.0,
                58.0,
                "Drawn by the host from a display list this plugin sent. The chart and the swatch are the only pixels.",
                0.0,
                palette.muted,
            )
            .text(28.0, 100.0, "Counter", 0.0, palette.muted)
            .text(28.0, 118.0, self.counter.to_string(), 32.0, palette.text)
            .button(NODE_DECREASE, PageRect::new(150.0, 124.0, 116.0, 36.0), "Decrease")
            .button(NODE_INCREASE, PageRect::new(278.0, 124.0, 116.0, 36.0), "Increase")
            .text(
                408.0,
                134.0,
                "Arrow up and down work too",
                0.0,
                palette.muted,
            )
            .checkbox(
                NODE_ANNOUNCE,
                28.0,
                190.0,
                "Announce the counter out loud",
                self.announce,
            )
            .text(28.0, 232.0, "Note", 0.0, palette.muted)
            .text_field(NODE_NOTE, PageRect::new(28.0, 252.0, 420.0, 36.0), "Note", self.note.clone());
        if self.note.is_empty() {
            // A placeholder is the page's own doing: the vocabulary has one
            // text colour per node, so an empty field says so in muted text
            // rather than the host inventing a hint it cannot know.
            page = page.text(
                36.0,
                262.0,
                "Type here and the page redraws",
                0.0,
                palette.muted,
            );
        }
        let status = format!(
            "{} characters typed - the counter is {}",
            self.note.chars().count(),
            if self.announce { "loud" } else { "quiet" }
        );
        page = page
            .text(
                28.0,
                302.0,
                status,
                0.0,
                if self.announce {
                    palette.accent
                } else {
                    palette.muted
                },
            )
            .button(NODE_CLOSE, PageRect::new(28.0, 330.0, 140.0, 36.0), "Close page")
            .text(
                184.0,
                340.0,
                "Escape closes it too: the host keeps that key.",
                0.0,
                palette.muted,
            );
        let counter = self.counter;
        page = page
            // Decoration, and left unlabelled to say so: a raster announces
            // nothing unless the author gives it a name.
            .image(
                PageRect::new(596.0, 96.0, 40.0, 40.0),
                "",
                SWATCH_EDGE,
                SWATCH_EDGE,
                SWATCH_RGBA.to_vec(),
            )
            .expect("the swatch carries its own dimensions in bytes")
            .text(480.0, 168.0, "Counter chart", 0.0, palette.muted)
            .canvas(
                PageRect::new(480.0, 190.0, 160.0, 48.0),
                format!("Counter chart, {counter}"),
                CHART_PIXEL_WIDTH,
                CHART_PIXEL_HEIGHT,
                |canvas| draw_counter_chart(canvas, counter, palette),
            )
            .expect("the chart's dimensions are constants");
        let pending_focus = std::mem::take(&mut self.pending_focus);
        if pending_focus != 0 {
            page = page.focus(pending_focus);
        }
        if self.closing {
            page = page.close();
        }
        page.build()
    }
}

/// Paints the counter as a chart that grows a column per unit.
///
/// Repainted from the current counter on every frame, which is the whole
/// point of it: a surface the plugin draws is worth having only if it can say
/// something a shipped picture cannot.
fn draw_counter_chart(canvas: &mut PageCanvas, counter: i64, palette: PagePalette) {
    let track = PageColor::rgba(palette.muted.r, palette.muted.g, palette.muted.b, 0x40);
    canvas.fill(track);
    // Negative counters and counters past the chart's width are the page's
    // problem, not the host's: a chart that grew without bound would be a
    // raster that grew without bound.
    let columns = counter.clamp(0, i64::from(CHART_COLUMNS)) as u32;
    for column in 0..columns {
        let bar = (canvas.height() * (column + 1)) / CHART_COLUMNS;
        canvas.fill_rect(
            column * (CHART_COLUMN_WIDTH + CHART_COLUMN_GAP),
            canvas.height() - bar,
            CHART_COLUMN_WIDTH,
            bar,
            palette.accent,
        );
    }
}

fn wait_for_cancellation(context: &dyn PluginContext, milliseconds: u64) -> bool {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(milliseconds))
        .unwrap_or_else(Instant::now);
    loop {
        if context.cancellation().is_cancelled() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn emit_final(sink: &mut dyn SuggestionSink, items: Vec<crikey_core::Item>) -> Result<()> {
    if !items.is_empty() {
        sink.emit_batch(items)?;
    }
    sink.finish()
}

#[derive(Debug)]
struct ConformancePlugin {
    mode: Mode,
    /// State of the demo page. Lives on the plugin rather than in a frame
    /// because a page is a conversation: the host asks for frame after frame
    /// and nothing it sends carries the page's own content back.
    page: PageState,
}

impl ConformancePlugin {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            page: PageState::default(),
        }
    }

    fn suggest_echo(&mut self, query: Query, sink: &mut dyn SuggestionSink) -> Result<()> {
        let first = result_item("echo-1", query.text.clone());
        let second = result_item("echo-2", format!("{} (second)", query.text));
        sink.emit_batch(vec![first])?;
        sink.emit_batch(vec![second])?;
        sink.finish()
    }

    fn suggest_stream(
        &mut self,
        query: Query,
        count: usize,
        pid_first: bool,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        let mut start = 0usize;
        while start < count {
            if sink.is_cancelled() {
                return sink.finish();
            }
            let end = start.saturating_add(16).min(count);
            let batch = (start..end)
                .map(|index| {
                    if pid_first && index == 0 {
                        pid_item()
                    } else {
                        result_item(format!("stream-{index}"), format!("{} #{index}", query.text))
                    }
                })
                .collect();
            sink.emit_batch(batch)?;
            start = end;
        }
        sink.finish()
    }

    fn suggest_acceptance(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        if query.text.starts_with("slow") {
            self.suggest_slow(query, 2_000, context, sink)
        } else {
            self.suggest_stream(query, 35, true, sink)
        }
    }

    fn suggest_slow(
        &mut self,
        query: Query,
        milliseconds: u64,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        if wait_for_cancellation(context, milliseconds) {
            return sink.finish();
        }
        emit_final(
            sink,
            vec![result_item("slow-1", format!("{} (slow)", query.text))],
        )
    }
    fn suggest_slow_witness(
        &mut self,
        query: Query,
        milliseconds: u64,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        let initial = sink.emit_batch(vec![result_item("slow-start", query.text.clone())]);
        if let Err(error) = initial {
            if context.cancellation().is_cancelled() || sink.is_cancelled() {
                return sink.finish();
            }
            return Err(error);
        }
        if wait_for_cancellation(context, milliseconds) {
            return sink.finish();
        }
        sink.finish()
    }
    fn suggest_ignore_cancel(
        &mut self,
        query: Query,
        milliseconds: u64,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        thread::sleep(Duration::from_millis(milliseconds));
        emit_final(
            sink,
            vec![result_item(
                "ignore-cancel-1",
                format!("{} (ignored)", query.text),
            )],
        )
    }
}

/// Dies the way a real native plugin dies: a diagnostic on stderr, then the
/// abort. The host's contract is that it CAPTURES the child's crash output
/// (§16.6, §26.1), so a fixture that aborts silently would let a host which
/// captures nothing still look correct.
fn crash(reason: &str) -> ! {
    use std::io::Write;
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "[crikey-conformance] fatal: {reason}");
    let _ = stderr.flush();
    std::process::abort();
}

impl Plugin for ConformancePlugin {
    fn start(&mut self, _context: &dyn PluginContext) -> Result<()> {
        if matches!(self.mode, Mode::CrashOnStart) {
            crash("crash-on-start requested");
        }
        Ok(())
    }

    fn build_catalog(&mut self, _context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        if let Mode::Stream(count) = self.mode.clone() {
            let mut batch = Vec::with_capacity(16);
            for catalog_item in catalog_stream_items(count) {
                batch.push(catalog_item);
                if batch.len() == 16 {
                    sink.emit_batch(std::mem::take(&mut batch))?;
                }
            }
            if !batch.is_empty() {
                sink.emit_batch(batch)?;
            }
            return sink.finish();
        }

        let items = match self.mode {
            Mode::EnvWitness => environment_items(),
            // Deliberately empty. The page item is a suggestion, and
            // publishing the same stable id in the catalog as well makes the
            // aggregator treat the suggestion as a duplicate of a catalog row
            // and drop it, so the item never reaches the launcher at all. A
            // plugin whose whole purpose is one page has nothing to catalog.
            Mode::Page => Vec::new(),
            _ => catalog_items(),
        };
        sink.emit_batch(items)?;
        sink.finish()
    }

    fn suggest(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        match self.mode.clone() {
            Mode::Echo | Mode::SameId => self.suggest_echo(query, sink),
            Mode::EnvWitness => emit_final(sink, environment_items()),
            Mode::Acceptance => self.suggest_acceptance(query, context, sink),
            Mode::Stream(count) => self.suggest_stream(query, count, false, sink),
            Mode::Slow(milliseconds) => self.suggest_slow(query, milliseconds, context, sink),
            Mode::SlowWitness(milliseconds) => self.suggest_slow_witness(query, milliseconds, context, sink),
            Mode::IgnoreCancel(milliseconds) => self.suggest_ignore_cancel(query, milliseconds, sink),
            Mode::CrashOnSuggest => crash("crash-on-suggest requested"),
            Mode::CrashOnStart => Err(invalid("crash-on-start should not receive suggest")),
            Mode::FailSuggest => Err(invalid("conformance fail-suggest requested")),
            Mode::Sequence => Err(invalid("sequence mode must be resolved before serving")),
            Mode::Icon => emit_final(sink, icon_items()),
            Mode::IconSlow => emit_final(sink, vec![icon_item("icon-after-window", ICON_AFTER_WINDOW)]),
            Mode::IconSilent => emit_final(sink, vec![icon_item("icon-never", ICON_NEVER)]),
            Mode::Page => emit_final(sink, vec![page_item()]),
        }
    }

    fn execute(&mut self, _request: ExecuteRequest, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    /// Opens the demo page when the page item is run, and behaves exactly as
    /// before for everything else: the outcome is what tells the host to keep
    /// the launcher on a surface instead of dismissing it (§32.2).
    fn execute_outcome(
        &mut self,
        request: ExecuteRequest,
        context: &dyn PluginContext,
    ) -> Result<ExecuteOutcome> {
        let opens_page = matches!(self.mode, Mode::Page)
            && request.item.0 == PAGE_ITEM_ID
            && request
                .action
                .as_ref()
                .is_none_or(|action| action.0 == PAGE_ACTION_ID);
        if opens_page {
            // A fresh session: the host sends `Opened` first, and this makes
            // the second visit look like the first rather than resuming a
            // counter the user has forgotten about.
            self.page = PageState::default();
            return Ok(ExecuteOutcome::show_page(PAGE_ID));
        }
        self.execute(request, context).map(ExecuteOutcome::from)
    }

    /// Draws one frame of the demo page.
    ///
    /// Every event is folded into the page's own state before anything is
    /// drawn, because the display list is a function of that state: drawing
    /// while events were still pending would show the user the frame before
    /// the one they asked for.
    fn page(&mut self, request: PageRequest, _context: &dyn PluginContext) -> Result<PageFrame> {
        if request.page_id != PAGE_ID {
            return Err(invalid(format!(
                "conformance plugin owns no page {}",
                request.page_id
            )));
        }
        for event in &request.events {
            self.page.apply(event);
        }
        Ok(self.page.frame(&request))
    }

    /// Serves the icon references the icon modes publish, one behaviour per
    /// reference. Every other mode leaves the SDK default in place, which is
    /// the "plugin serves no resources" case the host must also survive.
    fn resource(
        &mut self,
        kind: ResourceKind,
        reference: &str,
        _context: &dyn PluginContext,
    ) -> Result<Option<PluginResource>> {
        if !matches!(self.mode, Mode::Icon | Mode::IconSlow | Mode::IconSilent)
            || kind != ResourceKind::Icon
        {
            return Ok(None);
        }
        let content = match reference {
            ICON_SERVED => icon_bytes(0),
            ICON_OVERSIZED => icon_bytes(512 * 1024),
            ICON_LATE => {
                thread::sleep(ICON_LATE_DELAY);
                icon_bytes(0)
            }
            ICON_AFTER_WINDOW => {
                thread::sleep(ANSWERS_AFTER_THE_COLLECTION_WINDOW);
                icon_bytes(0)
            }
            ICON_NEVER => {
                thread::sleep(NEVER_ANSWERS);
                icon_bytes(0)
            }
            _ => return Ok(None),
        };
        Ok(Some(PluginResource {
            content,
            media_type: "image/svg+xml".to_owned(),
        }))
    }

    fn stop(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
}

fn run() -> Result<(), SdkError> {
    let mode = match parse_mode(&selected_mode()) {
        Mode::Sequence => sequence_stage(),
        mode => mode,
    };
    let configured_id = if matches!(mode, Mode::SameId) {
        SHARED_PLUGIN_ID
    } else {
        PLUGIN_ID
    };
    let mut config = ServeConfig::from_env(configured_id, PLUGIN_VERSION)?;
    if matches!(mode, Mode::SameId) {
        config.plugin_id = SHARED_PLUGIN_ID.to_owned();
    }
    config.plugin_name = PLUGIN_NAME.to_owned();
    config.capabilities.streaming_catalog = true;
    config.capabilities.streaming_suggestions = true;
    config.capabilities.cancellation = true;
    let mut plugin = ConformancePlugin::new(mode);
    serve(&mut plugin, config)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("crikey-conformance-plugin: {error:?}");
        std::process::exit(1);
    }
}

/// Proves the demo page is a working surface rather than a picture of one: the
/// state it holds, the events it honours and the frame it draws.
#[cfg(test)]
mod tests {
    use super::*;
    use crikey_core::{NodeRole, PageColor};
    use crikey_plugin_sdk::PagePalette;

    fn request(generation: u64) -> PageRequest {
        PageRequest {
            page_id: PAGE_ID.to_owned(),
            generation,
            width: 720,
            height: 400,
            events: Vec::new(),
            focused: true,
            palette: PagePalette {
                surface: PageColor::rgba(34, 38, 45, 255),
                text: PageColor::rgba(235, 238, 242, 255),
                accent: PageColor::rgba(232, 174, 88, 255),
                muted: PageColor::rgba(158, 167, 179, 255),
            },
        }
    }

    fn activate(node_id: u32) -> PageInput {
        PageInput {
            node_id,
            ..PageInput::new(PageInputKind::Activated)
        }
    }

    fn typed(text: &str) -> PageInput {
        PageInput {
            text: text.to_owned(),
            ..PageInput::new(PageInputKind::TextInput)
        }
    }

    fn key(name: &str) -> PageInput {
        PageInput {
            key: name.to_owned(),
            ..PageInput::new(PageInputKind::KeyPressed)
        }
    }

    fn draw(state: &mut PageState, generation: u64, events: &[PageInput]) -> PageFrame {
        for event in events {
            state.apply(event);
        }
        state.frame(&request(generation))
    }

    #[test]
    fn the_opening_frame_is_drawable_and_fully_announced() {
        let mut state = PageState::default();
        let frame = draw(&mut state, 1, &[PageInput::new(PageInputKind::Opened)]);
        frame.validate().expect("the demo page must be drawable");
        assert_eq!(frame.title, "Page Playground");
        assert!(!frame.close);
        assert_eq!(
            frame.unlabelled_interactive(),
            Vec::<u32>::new(),
            "every interactive node must carry a name assistive technology can read"
        );
        assert_eq!(
            frame.focus_ring(),
            vec![NODE_DECREASE, NODE_INCREASE, NODE_ANNOUNCE, NODE_NOTE, NODE_CLOSE]
        );
        assert_eq!(frame.focus_node, NODE_NOTE, "an opened page places the caret itself");
        assert_eq!(
            frame.node(NODE_ANNOUNCE).map(|node| node.role),
            Some(NodeRole::Checkbox)
        );
        assert_eq!(
            frame.node(NODE_NOTE).map(|node| node.role),
            Some(NodeRole::TextField)
        );

        // Focus is requested once. A page that asked again every frame would
        // pull the user back out of whatever they had tabbed to.
        let second = draw(&mut state, 2, &[]);
        assert_eq!(second.focus_node, 0);
    }

    #[test]
    fn the_counter_survives_frames_and_answers_both_buttons_and_keys() {
        let mut state = PageState::default();
        draw(&mut state, 1, &[PageInput::new(PageInputKind::Opened)]);
        draw(&mut state, 2, &[activate(NODE_INCREASE), activate(NODE_INCREASE)]);
        let frame = draw(&mut state, 3, &[activate(NODE_DECREASE), key("ArrowUp")]);
        assert!(
            frame.nodes.iter().any(|node| node.text == "2"),
            "the counter must be drawn from state held across frames"
        );
        let frame = draw(&mut state, 4, &[key("ArrowDown"), key("ArrowDown")]);
        assert!(frame.nodes.iter().any(|node| node.text == "0"));
    }

    #[test]
    fn the_checkbox_toggles_and_carries_its_state() {
        let mut state = PageState::default();
        draw(&mut state, 1, &[PageInput::new(PageInputKind::Opened)]);
        let frame = draw(&mut state, 2, &[activate(NODE_ANNOUNCE)]);
        assert_eq!(frame.node(NODE_ANNOUNCE).map(|node| node.checked), Some(true));
        let frame = draw(&mut state, 3, &[activate(NODE_ANNOUNCE)]);
        assert_eq!(frame.node(NODE_ANNOUNCE).map(|node| node.checked), Some(false));
    }

    #[test]
    fn typed_text_lands_in_the_field_only_while_it_has_focus() {
        let mut state = PageState::default();
        draw(&mut state, 1, &[PageInput::new(PageInputKind::Opened)]);
        let frame = draw(&mut state, 2, &[typed("hi"), typed("!")]);
        assert!(frame.nodes.iter().any(|node| node.text == "hi!"));

        let frame = draw(&mut state, 3, &[key("Backspace")]);
        assert!(frame.nodes.iter().any(|node| node.text == "hi"));

        // Focus moved to a button: the same keystrokes must not edit the note.
        let focus_button = PageInput {
            node_id: NODE_INCREASE,
            ..PageInput::new(PageInputKind::FocusChanged)
        };
        let frame = draw(&mut state, 4, &[focus_button, typed("x"), key("Backspace")]);
        assert!(frame.nodes.iter().any(|node| node.text == "hi"));
    }

    #[test]
    fn the_note_is_bounded_by_the_plugin() {
        let mut state = PageState::default();
        draw(&mut state, 1, &[PageInput::new(PageInputKind::Opened)]);
        let frame = draw(&mut state, 2, &[typed(&"a".repeat(MAX_NOTE_CHARS * 2))]);
        assert_eq!(state.note.chars().count(), MAX_NOTE_CHARS);
        frame.validate().expect("a bounded page stays drawable");
    }

    #[test]
    fn the_close_button_ends_the_page_from_the_plugin_side() {
        let mut state = PageState::default();
        draw(&mut state, 1, &[PageInput::new(PageInputKind::Opened)]);
        let frame = draw(&mut state, 2, &[activate(NODE_CLOSE)]);
        assert!(frame.close, "the page must end itself rather than wait for Escape");
    }

    #[test]
    fn the_chart_is_repainted_from_the_counter_and_the_swatch_is_not() {
        let mut state = PageState::default();
        let frame = draw(&mut state, 1, &[PageInput::new(PageInputKind::Opened)]);
        frame.validate().expect("a page carrying rasters stays drawable");
        let rasters = |frame: &PageFrame| -> Vec<crikey_core::PageImage> {
            frame
                .nodes
                .iter()
                .filter_map(|node| node.image.clone())
                .collect()
        };
        let opening = rasters(&frame);
        assert_eq!(opening.len(), 2, "the swatch and the chart both reach the host");
        assert_eq!(opening[0].rgba, SWATCH_RGBA.to_vec());

        let raised = rasters(&draw(&mut state, 2, &[activate(NODE_INCREASE)]));
        assert_eq!(
            raised[0].rgba, opening[0].rgba,
            "a shipped picture has no reason to change"
        );
        assert_ne!(
            raised[1].rgba, opening[1].rgba,
            "the chart must be painted from the counter, not blitted once"
        );
        // Bottom-left pixel: the shortest column starts there, so it is the
        // one pixel that has to change when the counter leaves zero.
        let accent = request(2).palette.accent;
        let bottom_left = ((CHART_PIXEL_HEIGHT - 1) * CHART_PIXEL_WIDTH * 4) as usize;
        assert_eq!(
            raised[1].rgba[bottom_left..bottom_left + 4],
            [accent.r, accent.g, accent.b, accent.a],
            "the first column must be painted in the host's accent colour"
        );
        assert_ne!(
            opening[1].rgba[bottom_left..bottom_left + 4],
            [accent.r, accent.g, accent.b, accent.a],
            "a counter of zero must draw no column at all"
        );
    }

    #[test]
    fn only_the_page_this_fixture_owns_is_drawn() {
        let mut plugin = ConformancePlugin::new(Mode::Page);
        let mut foreign = request(1);
        foreign.page_id = "someone-elses".to_owned();
        let context = TestContext;
        assert!(plugin.page(foreign, &context).is_err());
        assert!(plugin.page(request(1), &context).is_ok());
    }

    #[derive(Debug)]
    struct TestContext;

    impl crikey_plugin_sdk::CancellationToken for TestContext {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    impl PluginContext for TestContext {
        fn plugin_id(&self) -> &crikey_core::PluginId {
            static ID: std::sync::LazyLock<crikey_core::PluginId> =
                std::sync::LazyLock::new(|| crikey_core::PluginId(PLUGIN_ID.to_owned()));
            &ID
        }

        fn cancellation(&self) -> &dyn crikey_plugin_sdk::CancellationToken {
            self
        }

        fn log(&self, _level: crikey_plugin_sdk::LogLevel, _message: &str) {}
    }
}
