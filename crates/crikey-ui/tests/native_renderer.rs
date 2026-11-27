use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use crikey_core::{
    Generation, GenerationTracker, ItemId, NodeRole, NodeShape, PageColor, PageFrame, PageImage, PageInput,
    PageInputKind, PageNode, PluginId,
};
use crikey_platform::IconImage;
use crikey_ui::{
    build_launcher_frame, create_launcher_context, egui, ActivationLatencyTracker, LauncherViewModel,
    NativeLauncher, NativeLauncherConfig, NativeLauncherHandle, PageSurface, RendererError, ResultRow,
    SettingControl, SettingRow, UiCommand, ViewModel, WebImeEvent, WebSurfaceFrame, WebSurfaceInput,
    ACTIVATION_SAMPLE_CAPACITY, WEB_MODIFIER_SHIFT,
};

fn model(query: &str) -> ViewModel {
    let mut owner = crikey_ui::LauncherViewModel::new();
    owner.activate();
    ViewModel {
        view: owner.view().map(|(view, _)| view),
        input_receipt: None,
        generation: Generation::ZERO,
        query: query.to_owned(),
        rows: Arc::default(),
        selected: 0,
        root_selection_revision: 0,
        pending_plugins: false,
        action_panel: None,
        settings_open: false,
        settings: Arc::default(),
        setting_edits: Arc::default(),
        command_management: Arc::default(),
        settings_focus: None,
        page: None,
        show_hints: true,
        rounded_corners: true,
        preview: Default::default(),
        navigation: Default::default(),
    }
}

fn result_row(index: usize) -> ResultRow {
    ResultRow {
        target: crikey_ui::ActionTarget::Result {
            owner: PluginId("renderer-fixture".into()),
            item: ItemId(format!("item-{index}")),
        },
        source: Some(crikey_ui::ResultSource {
            owner: PluginId("renderer-fixture".into()),
            item: ItemId(format!("item-{index}")),
            kind: crikey_ui::ResultSourceKind::Ordinary,
        }),
        command_state: None,
        label: format!("row-{index}"),
        description: String::new(),
        icon_reference: None,
        icon: None,
        category: "application".to_owned(),
        plugin_name: "core".to_owned(),
        highlights: Vec::new(),
        argument_hint: None,
        status: None,
        default_action: None,
        alternate_actions: Vec::new(),
    }
}

fn contains_text(shape: &egui::Shape, needle: &str) -> bool {
    match shape {
        egui::Shape::Text(text) => text.galley.text().contains(needle),
        egui::Shape::Vec(shapes) => shapes.iter().any(|shape| contains_text(shape, needle)),
        _ => false,
    }
}

#[test]
fn native_handle_is_safe_for_platform_hotkey_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NativeLauncherHandle>();
}

#[test]
fn invalid_window_dimensions_are_reported_before_display_initialization() {
    let mut config = NativeLauncherConfig::default();
    let expected_height = config.height;
    config.width = 0;
    let error = NativeLauncher::new(config).expect_err("a zero-sized surface configuration must be rejected");

    assert!(matches!(
        error,
        RendererError::InvalidWindowSize {
            width: 0,
            height
        } if height == expected_height
    ));
}

#[test]
fn headless_frame_contains_the_latest_query_text() {
    let context = create_launcher_context();
    let window = NativeLauncherConfig::default();
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(window.width as f32, window.height as f32),
        )),
        focused: true,
        ..Default::default()
    };

    let frame = build_launcher_frame(&context, input, &model("terminal settings"));

    assert!(
        frame
            .output
            .shapes
            .iter()
            .any(|clipped| contains_text(&clipped.shape, "terminal settings")),
        "the query accepted by the immutable view model must be painted in the next egui frame"
    );
}

#[test]
fn latency_tracker_reports_nearest_rank_p95_without_unbounded_retention() {
    let mut tracker = ActivationLatencyTracker::new();
    for milliseconds in 1..=100 {
        tracker.observe(Duration::from_millis(milliseconds));
    }

    let first = tracker.snapshot();
    assert_eq!(first.total_samples, 100);
    assert_eq!(first.retained_samples, 100);
    assert_eq!(first.latest, Some(Duration::from_millis(100)));
    assert_eq!(first.p95, Some(Duration::from_millis(95)));

    for milliseconds in 101..=(ACTIVATION_SAMPLE_CAPACITY as u64 + 37) {
        tracker.observe(Duration::from_millis(milliseconds));
    }

    let wrapped = tracker.snapshot();
    assert_eq!(wrapped.total_samples, ACTIVATION_SAMPLE_CAPACITY as u64 + 37);
    assert_eq!(wrapped.retained_samples, ACTIVATION_SAMPLE_CAPACITY);
    assert_eq!(
        wrapped.latest,
        Some(Duration::from_millis(ACTIVATION_SAMPLE_CAPACITY as u64 + 37))
    );
}

#[test]
fn a_zero_sized_window_still_builds_a_frame() {
    let context = create_launcher_context();
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::ZERO)),
        focused: true,
        ..Default::default()
    };

    // A minimised or freshly mapped window can report no area at all. Laying
    // the launcher out inside it must not divide by the missing height or
    // panic on a negative size.
    let frame = build_launcher_frame(&context, input, &model(""));

    assert!(frame.commands.is_empty());
}

#[test]
fn the_result_count_is_worded_for_one_result_and_for_several() {
    fn status_shows(rows: usize, needle: &str) -> bool {
        let context = create_launcher_context();
        let window = NativeLauncherConfig::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(window.width as f32, window.height as f32),
            )),
            focused: true,
            ..Default::default()
        };
        let mut view = model("q");
        view.rows = (0..rows).map(result_row).collect();

        let frame = build_launcher_frame(&context, input, &view);
        frame
            .output
            .shapes
            .iter()
            .any(|clipped| contains_text(&clipped.shape, needle))
    }

    assert!(status_shows(1, "1 result"));
    assert!(
        !status_shows(1, "1 results"),
        "a single result must not be reported in the plural"
    );
    assert!(status_shows(2, "2 results"));
}

// ---------------------------------------------------------------------------
// Icons (spec 6.4)
// ---------------------------------------------------------------------------

/// An opaque solid-colour icon.
///
/// Opaque on purpose: `egui` stores premultiplied colour, so an opaque pixel is
/// the one case where the uploaded bytes must equal the decoded ones and the
/// assertion can be an equality rather than a tolerance.
fn solid_icon(colour: [u8; 4]) -> Arc<IconImage> {
    let rgba = colour
        .iter()
        .copied()
        .cycle()
        .take(ICON_EDGE * ICON_EDGE * 4)
        .collect();
    Arc::new(
        IconImage::new("test-icon", ICON_EDGE as u32, ICON_EDGE as u32, rgba)
            .expect("a solid icon is well formed"),
    )
}

/// Deliberately not the size of the row's icon slot: the frame has to carry the
/// decoded extent, and a slot-sized fixture could not tell the two apart.
const ICON_EDGE: usize = 6;

fn frame_of(context: &egui::Context, model: &ViewModel) -> egui::FullOutput {
    let window = NativeLauncherConfig::default();
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(window.width as f32, window.height as f32),
        )),
        focused: true,
        ..Default::default()
    };
    build_launcher_frame(context, input, model).output
}

fn one_row_model(row: ResultRow) -> ViewModel {
    let mut view = model("row");
    view.rows = Arc::from(vec![row]);
    view
}

/// Every textured rectangle in a frame, with the texture it samples.
fn textured_rects(shape: &egui::Shape, found: &mut Vec<(egui::TextureId, egui::Rect)>) {
    match shape {
        egui::Shape::Rect(rect) if rect.fill_texture_id != egui::TextureId::default() => {
            found.push((rect.fill_texture_id, rect.rect));
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                textured_rects(shape, found);
            }
        }
        _ => {}
    }
}

/// Where the label of a row was laid out.
fn text_origin(shape: &egui::Shape, needle: &str) -> Option<egui::Pos2> {
    match shape {
        egui::Shape::Text(text) if text.galley.text().contains(needle) => Some(text.pos),
        egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| text_origin(shape, needle)),
        _ => None,
    }
}

#[test]
fn a_row_icon_is_uploaded_as_a_texture_and_painted_beside_the_label() {
    let context = create_launcher_context();
    let colour = [17, 34, 51, 255];
    let mut row = result_row(0);
    row.icon = Some(solid_icon(colour));

    let output = frame_of(&context, &one_row_model(row));

    // The decoded pixels reached the texture upload path the renderer feeds to
    // `Renderer::update_texture`.
    let (id, delta) = output
        .textures_delta
        .set
        .iter()
        .find(|(_, delta)| delta.image.size() == [ICON_EDGE, ICON_EDGE])
        .expect("the icon's pixels are uploaded as a texture of its decoded extent");
    match &delta.image {
        egui::ImageData::Color(image) => assert_eq!(
            image.pixels[0],
            egui::Color32::from_rgba_premultiplied(colour[0], colour[1], colour[2], colour[3]),
            "the uploaded texture carries the decoded pixels, unaltered"
        ),
        egui::ImageData::Font(_) => panic!("an icon uploads colour data, not font coverage"),
    }

    // And that texture is what a rectangle in the painted frame samples.
    let mut painted = Vec::new();
    for clipped in &output.shapes {
        textured_rects(&clipped.shape, &mut painted);
    }
    let icon = painted
        .iter()
        .find(|(painted, _)| painted == id)
        .expect("the uploaded icon texture is painted in this frame");
    assert!(
        icon.1.width() > 0.0 && icon.1.height() > 0.0,
        "the icon is painted into a rectangle with area, got {:?}",
        icon.1
    );

    let label = output
        .shapes
        .iter()
        .find_map(|clipped| text_origin(&clipped.shape, "row-0"))
        .expect("the row label is painted");
    assert!(
        icon.1.max.x <= label.x,
        "the icon is drawn beside the label rather than over it: icon ends at {}, label starts at {}",
        icon.1.max.x,
        label.x
    );
}

#[test]
fn a_row_with_no_icon_leaves_the_label_exactly_where_an_icon_would_have_put_it() {
    let context = create_launcher_context();
    let mut with_icon = result_row(0);
    with_icon.icon = Some(solid_icon([17, 34, 51, 255]));
    let without_icon = result_row(0);

    // A launcher whose rows shift because one icon 404s is worse than one with no
    // icons, so the slot is reserved whether or not it is filled. The two frames
    // are built in separate contexts so that neither can be reading the other's
    // retained layout.
    let placed = frame_of(&create_launcher_context(), &one_row_model(with_icon));
    let missing = frame_of(&context, &one_row_model(without_icon));

    let origin = |output: &egui::FullOutput| {
        output
            .shapes
            .iter()
            .find_map(|clipped| text_origin(&clipped.shape, "row-0"))
            .expect("the row label is painted")
    };

    assert_eq!(origin(&placed), origin(&missing));
}

#[test]
fn one_icon_shown_twice_is_uploaded_once() {
    let context = create_launcher_context();
    let icon = solid_icon([17, 34, 51, 255]);
    let mut view = model("row");
    let rows: Vec<ResultRow> = (0..2)
        .map(|index| {
            let mut row = result_row(index);
            row.icon = Some(Arc::clone(&icon));
            row
        })
        .collect();
    view.rows = Arc::from(rows);

    let output = frame_of(&context, &view);

    // Two rows, one upload: the texture cache is keyed on the pixels, so the
    // same icon on every row of a large result set costs one texture.
    let uploads = output
        .textures_delta
        .set
        .iter()
        .filter(|(_, delta)| delta.image.size() == [ICON_EDGE, ICON_EDGE])
        .count();
    assert_eq!(uploads, 1);
}

#[test]
fn a_second_frame_reuses_the_texture_the_first_one_uploaded() {
    let context = create_launcher_context();
    let mut row = result_row(0);
    row.icon = Some(solid_icon([17, 34, 51, 255]));
    let view = one_row_model(row);

    let first = frame_of(&context, &view);
    let second = frame_of(&context, &view);

    let icon_uploads = |output: &egui::FullOutput| {
        output
            .textures_delta
            .set
            .iter()
            .filter(|(_, delta)| delta.image.size() == [ICON_EDGE, ICON_EDGE])
            .count()
    };
    assert_eq!(icon_uploads(&first), 1, "the first frame uploads the icon");
    assert_eq!(
        icon_uploads(&second),
        0,
        "a steady-state frame must not re-upload an icon it already holds"
    );
}

// ---------------------------------------------------------------------------
// The untyped launcher, list scrolling and the settings surface: the three
// things the first Windows tester found unusable.
// ---------------------------------------------------------------------------

fn launcher_input(events: Vec<egui::Event>) -> egui::RawInput {
    let window = NativeLauncherConfig::default();
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(window.width as f32, window.height as f32),
        )),
        focused: true,
        events,
        ..Default::default()
    }
}

fn force_three_layout_passes(context: &egui::Context, change_geometry: bool) {
    context.options_mut(|options| options.max_passes = 3.try_into().expect("three passes are nonzero"));
    let passes = AtomicUsize::new(0);
    context.on_begin_pass(
        "native-renderer multipass regression",
        Arc::new(move |context| {
            let pass = passes.fetch_add(1, Ordering::Relaxed);
            if change_geometry && (1..=2).contains(&pass) {
                egui::TopBottomPanel::top("regression-layout")
                    .exact_height(30.0 * pass as f32)
                    .show(context, |ui| {
                        ui.label("Layout refined");
                    });
            }
        }),
    );
    let completed = AtomicUsize::new(0);
    context.on_end_pass(
        "native-renderer discard regression",
        Arc::new(move |context| {
            if completed.fetch_add(1, Ordering::Relaxed) < 2 {
                context.request_discard("exercise first-pass input and final-pass geometry");
            }
        }),
    );
}

#[test]
fn query_edits_survive_layout_passes_without_being_replayed() {
    let context = create_launcher_context();
    let view = model("");
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    force_three_layout_passes(&context, false);
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            egui::Event::Text("a".to_owned()),
            egui::Event::Text("a".to_owned()),
        ]),
        &view,
    );
    assert_eq!(frame.output.platform_output.num_completed_passes, 3);
    assert_eq!(frame.commands, vec![UiCommand::SetQuery("aa".to_owned())]);
}

fn painted(frame: &crikey_ui::NativeUiFrame, needle: &str) -> bool {
    frame
        .output
        .shapes
        .iter()
        .any(|clipped| contains_text(&clipped.shape, needle))
}

/// A press and release in one frame, which is what egui reads as a click.
fn click_at(position: egui::Pos2) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(position),
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        },
    ]
}

/// Where a piece of painted text sits, panicking with the searched text rather
/// than with `None` so a layout change names itself.
#[track_caller]
fn position_of(frame: &crikey_ui::NativeUiFrame, needle: &str) -> egui::Pos2 {
    frame
        .output
        .shapes
        .iter()
        .find_map(|clipped| text_origin(&clipped.shape, needle))
        .unwrap_or_else(|| panic!("{needle:?} is not painted in this frame"))
}

/// Reach a settings control through real wheel input, respecting nested body
/// viewports. Exact matching distinguishes the checkbox from its position label.
fn reveal_settings_text(
    context: &egui::Context,
    view: &ViewModel,
    mut frame: crikey_ui::NativeUiFrame,
    needle: &str,
    exact: bool,
) -> (crikey_ui::NativeUiFrame, egui::Pos2) {
    fn origin(shape: &egui::Shape, needle: &str, exact: bool) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(text)
                if (exact && text.galley.text() == needle)
                    || (!exact && text.galley.text().contains(needle)) =>
            {
                Some(text.pos)
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| origin(shape, needle, exact)),
            _ => None,
        }
    }
    for _ in 0..32 {
        if let Some(position) = frame.output.shapes.iter().find_map(|clipped| {
            let position = origin(&clipped.shape, needle, exact)?;
            clipped
                .clip_rect
                .contains(position + egui::vec2(4.0, 4.0))
                .then_some(position)
        }) {
            return (frame, position);
        }
        let quit = position_of(&frame, "Quit CriKey");
        let settings = position_of(&frame, "Settings");
        frame = build_launcher_frame(
            context,
            launcher_input(vec![
                egui::Event::PointerMoved(egui::pos2(settings.x + 64.0, quit.y - 24.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -120.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]),
            view,
        );
    }
    panic!("settings control {needle:?} was not reachable by body scrolling");
}

fn scrollable_model(rows: usize, selected: usize) -> ViewModel {
    let mut view = model("row");
    view.rows = (0..rows).map(result_row).collect();
    view.selected = selected;
    view
}

#[test]
fn admitted_home_rows_render_with_an_empty_query() {
    let context = create_launcher_context();
    let mut view = model("");
    // Publication admission belongs to the view model, not the renderer.
    view.rows = (0..3).map(result_row).collect();

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    assert!(painted(&frame, "row-0"));
    assert!(painted(&frame, "row-1"));
    assert!(painted(&frame, "row-2"));
    assert!(painted(&frame, "Search apps, files, and actions"));
}

/// A search that matched nothing must say so -- but in the status line, not in
/// a card under the field. The card made the window grow into a large empty
/// panel on the first keystroke and stay one until results arrived, which is
/// the "blocky" the first Windows tester reported.
#[test]
fn a_typed_query_with_no_matches_says_so_without_growing_a_panel() {
    let context = create_launcher_context();

    let empty = build_launcher_frame(&context, launcher_input(Vec::new()), &model("qqq"));
    assert!(
        painted(&empty, "0 results"),
        "a search that matched nothing must still tell the user that"
    );

    let mut pending = model("qqq");
    pending.pending_plugins = true;
    let pending = build_launcher_frame(&context, launcher_input(Vec::new()), &pending);
    assert!(
        painted(&pending, "Providers are still responding"),
        "a search still running must say so rather than look like it found nothing"
    );

    for absent in ["No matches", "Try fewer words", "Results will appear"] {
        assert!(
            !painted(&empty, absent) && !painted(&pending, absent),
            "nothing is drawn under the field until there are rows, yet it painted {absent:?}"
        );
    }
}

/// One egui context plus a monotonic clock.
///
/// egui animates a scroll over time rather than jumping to it, so a test that
/// wants to see where the list came to rest has to hand it a clock that moves
/// and enough frames for the animation to finish.
struct Frames {
    context: egui::Context,
    clock: f64,
}

impl Frames {
    fn new() -> Self {
        Self {
            context: create_launcher_context(),
            clock: 0.0,
        }
    }

    fn draw(&mut self, view: &ViewModel, events: Vec<egui::Event>) -> crikey_ui::NativeUiFrame {
        self.clock += 0.05;
        let mut input = launcher_input(events);
        input.time = Some(self.clock);
        build_launcher_frame(&self.context, input, view)
    }

    /// Draws until every animation has run out, and answers with the last
    /// frame.
    fn settle(&mut self, view: &ViewModel) -> crikey_ui::NativeUiFrame {
        let mut last = self.draw(view, Vec::new());
        for _ in 0..40 {
            last = self.draw(view, Vec::new());
        }
        last
    }

    /// Drives real paints and applies every emitted command to the model.
    fn settle_model(
        &mut self,
        state: &mut LauncherViewModel,
        view: &mut ViewModel,
    ) -> crikey_ui::NativeUiFrame {
        let mut last = None;
        for _ in 0..41 {
            let mut frame = self.draw(view, Vec::new());
            for command in frame.commands.drain(..) {
                let _ = state.apply(command);
            }
            if let Some(next) = state.frame() {
                *view = next;
            }
            last = Some(frame);
        }
        last.expect("at least one consumer frame")
    }
}

/// Reach inline action-sheet controls using its real, independently scrolling
/// viewport, excluding matching shortcut metadata in the root result list.
fn reveal_action_text(
    context: &egui::Context,
    view: &ViewModel,
    needle: &str,
) -> (crikey_ui::NativeUiFrame, egui::Pos2) {
    let mut frames = Frames {
        context: context.clone(),
        clock: context.input(|input| input.time),
    };
    let initial = frames.settle(view);
    let back = position_of(&initial, "Back");
    let pointer = back + egui::vec2(4.0, 100.0);
    let wheel = |delta| {
        vec![
            egui::Event::PointerMoved(pointer),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, delta),
                modifiers: egui::Modifiers::NONE,
            },
        ]
    };
    let _ = frames.draw(view, wheel(10000.0));
    for _ in 0..32 {
        let frame = frames.settle(view);
        let point = frame
            .output
            .shapes
            .iter()
            .filter_map(|clipped| {
                let point = text_origin(&clipped.shape, needle)? + egui::vec2(4.0, 4.0);
                (point.y > back.y + 32.0 && clipped.clip_rect.contains(point)).then_some(point)
            })
            .next_back();
        if let Some(point) = point {
            return (frame, point);
        }
        let _ = frames.draw(view, wheel(-80.0));
    }
    panic!("action-sheet control {needle:?} was not reachable by scrolling");
}

/// The wheel, over the middle of the result list.
fn wheel_over_list() -> Vec<egui::Event> {
    let over_list = egui::Pos2::new(200.0, 300.0);
    vec![
        egui::Event::PointerMoved(over_list),
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -400.0),
            modifiers: egui::Modifiers::NONE,
        },
    ]
}

#[test]
fn a_mouse_scroll_is_not_undone_while_the_selection_stays_put() {
    let mut frames = Frames::new();
    let view = scrollable_model(40, 0);

    let first = frames.draw(&view, Vec::new());
    assert!(painted(&first, "row-0"), "the list starts at the top");

    let _ = frames.draw(&view, wheel_over_list());
    let settled = frames.settle(&view);

    assert!(
        !painted(&settled, "row-0"),
        "the repaints after a wheel gesture must not walk the list back to the selected row"
    );
}

#[test]
fn moving_the_selection_scrolls_the_row_back_into_view() {
    let mut frames = Frames::new();
    let view = scrollable_model(40, 0);
    let _ = frames.settle(&view);

    let far = scrollable_model(40, 39);
    let followed = frames.settle(&far);

    assert!(
        painted(&followed, "row-39"),
        "keyboard navigation must bring the selected row into view"
    );
}

#[test]
fn a_new_query_puts_the_selected_row_back_on_screen() {
    let mut frames = Frames::new();
    let view = scrollable_model(40, 0);
    let _ = frames.draw(&view, Vec::new());
    let _ = frames.draw(&view, wheel_over_list());
    let scrolled = frames.settle(&view);
    assert!(!painted(&scrolled, "row-0"));

    // A new query is navigation even when its rows and index are identical.
    let mut replaced = scrollable_model(40, 0);
    replaced.generation = GenerationTracker::new().advance();
    let followed = frames.settle(&replaced);

    assert!(
        painted(&followed, "row-0"),
        "a new query must show its selected row"
    );
}

/// Measures a full-width row against its actual paint viewport, not merely a
/// text galley that egui may emit outside the viewport.
fn root_row_geometry(frame: &crikey_ui::NativeUiFrame, label: &str) -> Option<(egui::Rect, egui::Rect)> {
    fn origin(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == label => Some(text.pos),
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| origin(shape, label)),
            _ => None,
        }
    }
    fn row_at(shape: &egui::Shape, position: egui::Pos2) -> Option<egui::Rect> {
        match shape {
            egui::Shape::Rect(rect)
                if rect.rect.width() > NativeLauncherConfig::default().width as f32 / 2.0
                    && rect.rounding.nw > 0.0
                    && rect.rect.contains(position) =>
            {
                Some(rect.rect)
            }
            egui::Shape::Vec(shapes) => shapes
                .iter()
                .filter_map(|shape| row_at(shape, position))
                .min_by(|a, b| a.height().total_cmp(&b.height())),
            _ => None,
        }
    }
    let position = frame
        .output
        .shapes
        .iter()
        .find_map(|shape| origin(&shape.shape, label))?;
    frame
        .output
        .shapes
        .iter()
        .filter_map(|shape| row_at(&shape.shape, position).map(|rect| (rect, shape.clip_rect)))
        .min_by(|(a, _), (b, _)| a.height().total_cmp(&b.height()))
}

fn root_row_visible(frame: &crikey_ui::NativeUiFrame, label: &str) -> bool {
    root_row_geometry(frame, label).is_some_and(|(row, viewport)| viewport.contains_rect(row))
}

fn root_state(generation: Generation) -> (LauncherViewModel, ViewModel) {
    let mut state = LauncherViewModel::new();
    state.activate();
    let _ = state.apply(UiCommand::SetQuery("root".into()));
    state.begin_generation(generation);
    state.publish(generation, (0..40).map(result_row).collect(), false);
    let view = state.frame().expect("initial root publication");
    (state, view)
}

#[test]
fn root_manual_scroll_survives_identical_and_cosmetic_publications() {
    let generation = GenerationTracker::new().advance();
    let (mut state, mut view) = root_state(generation);
    let mut frames = Frames::new();
    let first = frames.settle_model(&mut state, &mut view);
    assert!(root_row_visible(&first, "row-0"));
    let _ = frames.draw(&view, wheel_over_list());
    let manual = frames.settle_model(&mut state, &mut view);
    assert!(!root_row_visible(&manual, "row-0"));
    let (anchor, before) = view
        .rows
        .iter()
        .enumerate()
        .find_map(|(index, row)| {
            root_row_geometry(&manual, &row.label)
                .filter(|(rect, viewport)| viewport.contains_rect(*rect))
                .map(|(rect, _)| (index, rect))
        })
        .expect("manual viewport contains a complete result row");
    let old_rows = Arc::clone(&view.rows);
    let revision = view.root_selection_revision;
    state.publish(generation, old_rows.to_vec(), true);
    state.publish(generation, old_rows.to_vec(), false);
    view = state.frame().expect("coalesced host republications");
    assert!(
        !Arc::ptr_eq(&old_rows, &view.rows),
        "exercise a genuinely new allocation"
    );
    assert_eq!(view.root_selection_revision, revision);
    let republished = frames.settle_model(&mut state, &mut view);
    assert!(!root_row_visible(&republished, "row-0"));
    assert_eq!(
        root_row_geometry(&republished, &view.rows[anchor].label)
            .unwrap()
            .0,
        before
    );

    // Preserving manual scroll must not discard changed display content.
    let mut changed = view.rows.to_vec();
    changed[anchor].label = "Updated visible result".into();
    changed[anchor].description = "Updated description".into();
    changed[anchor].icon = Some(solid_icon([17, 34, 51, 255]));
    changed[anchor].status = Some("Updated status".into());
    state.publish(generation, changed, false);
    view = state.frame().expect("real cosmetic changes publish");
    assert_eq!(view.root_selection_revision, revision);
    assert_eq!(view.rows[anchor].status.as_deref(), Some("Updated status"));
    let updated = frames.settle_model(&mut state, &mut view);
    assert!(root_row_visible(&updated, "Updated visible result"));
    assert_eq!(
        root_row_geometry(&updated, "Updated visible result").unwrap().0,
        before
    );
    assert!(!root_row_visible(&updated, "row-0"));
}

#[test]
fn root_boundary_navigation_and_new_query_reveal_same_index_selection() {
    let generations = GenerationTracker::new();
    let generation = generations.advance();
    let (mut state, mut view) = root_state(generation);
    let mut frames = Frames::new();
    let _ = frames.settle_model(&mut state, &mut view);
    let _ = frames.draw(&view, wheel_over_list());
    let manual = frames.settle_model(&mut state, &mut view);
    assert!(!root_row_visible(&manual, "row-0"));
    let revision = view.root_selection_revision;
    let _ = state.apply(UiCommand::SelectPrevious);
    state.publish(generation, view.rows.to_vec(), false);
    view = state
        .frame()
        .expect("boundary navigation survives coalesced publication");
    assert_eq!(view.selected, 0);
    assert_ne!(view.root_selection_revision, revision);
    let followed = frames.settle_model(&mut state, &mut view);
    assert!(root_row_visible(&followed, "row-0"));

    let _ = frames.draw(&view, wheel_over_list());
    let manual = frames.settle_model(&mut state, &mut view);
    assert!(!root_row_visible(&manual, "row-0"));
    let _ = state.apply(UiCommand::SetQuery("another root query".into()));
    let next = generations.advance();
    state.begin_generation(next);
    state.publish(next, view.rows.to_vec(), false);
    view = state.frame().expect("new query with identical rows");
    let queried = frames.settle_model(&mut state, &mut view);
    assert_eq!(view.selected, 0);
    assert!(root_row_visible(&queried, "row-0"));
}

#[test]
fn root_keyboard_follow_survives_coalescing_and_same_index_target_replacement() {
    let generation = GenerationTracker::new().advance();
    let (mut state, mut view) = root_state(generation);
    let mut frames = Frames::new();
    let _ = frames.settle_model(&mut state, &mut view);
    let _ = state.apply(UiCommand::PageDown);
    let _ = state.apply(UiCommand::PageDown);
    let _ = state.apply(UiCommand::PageDown);
    state.publish(generation, view.rows.to_vec(), true);
    state.publish(generation, view.rows.to_vec(), false);
    view = state
        .frame()
        .expect("keyboard intent and host publications coalesce");
    let selected = view.selected;
    assert!(selected > 0);
    let followed = frames.settle_model(&mut state, &mut view);
    assert!(root_row_visible(&followed, &view.rows[selected].label));

    let _ = frames.draw(&view, wheel_over_list());
    let manual = frames.settle_model(&mut state, &mut view);
    assert!(!root_row_visible(&manual, &view.rows[selected].label));
    let revision = view.root_selection_revision;
    let mut changed = view.rows.to_vec();
    changed[selected] = result_row(1000 + selected);
    changed[selected].label = "Replacement selected result".into();
    state.publish(generation, changed, false);
    view = state
        .frame()
        .expect("removed selection falls back at the same index");
    assert_eq!(view.selected, selected);
    assert_ne!(view.root_selection_revision, revision);
    let replaced = frames.settle_model(&mut state, &mut view);
    assert!(root_row_visible(&replaced, "Replacement selected result"));
}

#[test]
fn root_first_provider_answer_follows_new_query_before_host_publication() {
    let generations = GenerationTracker::new();
    let (mut state, _) = root_state(generations.advance());
    let _ = state.apply(UiCommand::PageDown);
    let mut view = state.frame().expect("a nonzero choice in the previous query");
    let mut frames = Frames::new();
    let _ = frames.settle_model(&mut state, &mut view);
    let _ = frames.draw(&view, wheel_over_list());
    let manual = frames.settle_model(&mut state, &mut view);
    assert!(!root_row_visible(&manual, "row-0"));

    let _ = state.apply(UiCommand::SetQuery("next query".into()));
    let generation = generations.advance();
    state.begin_generation(generation);
    let pending = state.frame().expect("pending host query frame");
    assert_eq!(pending.root_selection_revision, view.root_selection_revision);
    // Provider first-answer delivery can draw before the host merges its rows.
    // Its host overlay carries the pending intent, not a fabricated new token.
    let mut provider = scrollable_model(40, 0);
    for (index, row) in Arc::make_mut(&mut provider.rows).iter_mut().enumerate() {
        *row = result_row(40 + index);
        row.label = format!("row-{index}");
    }
    provider.generation = generation;
    provider.root_selection_revision = pending.root_selection_revision;
    let answered = frames.settle(&provider);
    assert!(root_row_visible(&answered, "row-0"));

    // A subsequent identical first host merge is publication, not another
    // navigation intent that can revoke a wheel gesture after the answer.
    let _ = frames.draw(&provider, wheel_over_list());
    let manual = frames.settle(&provider);
    assert!(!root_row_visible(&manual, "row-0"));
    state.publish(generation, provider.rows.to_vec(), false);
    view = state.frame().expect("first host merge after provider answer");
    assert_eq!(view.root_selection_revision, provider.root_selection_revision);
    let merged = frames.settle_model(&mut state, &mut view);
    assert!(!root_row_visible(&merged, "row-0"));
}

fn hotkey_settings_model() -> ViewModel {
    let mut view = model("");
    view.settings_open = true;
    view.settings = Arc::from(vec![SettingRow {
        key: "launcher.activation-hotkey".to_owned(),
        label: "Activation hotkey".to_owned(),
        value: "Ctrl+Alt+Space".to_owned(),
        source: "default".to_owned(),
        control: SettingControl::Text,
    }]);
    view
}

#[test]
fn ctrl_comma_asks_for_the_settings_surface() {
    let context = create_launcher_context();
    let shortcut = vec![egui::Event::Key {
        key: egui::Key::Comma,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    }];

    force_three_layout_passes(&context, false);
    let frame = build_launcher_frame(&context, launcher_input(shortcut), &model(""));

    assert_eq!(frame.output.platform_output.num_completed_passes, 3);
    assert_eq!(frame.commands, vec![UiCommand::OpenSettings]);
}

#[test]
fn the_footer_offers_the_settings_surface_to_a_user_who_knows_no_shortcut() {
    let context = create_launcher_context();
    let view = model("");

    let located = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let affordance = position_of(&located, "Settings");

    let clicked = build_launcher_frame(
        &context,
        launcher_input(click_at(affordance + egui::vec2(4.0, 4.0))),
        &view,
    );

    assert!(clicked.commands.contains(&UiCommand::OpenSettings));
}

/// The hint line is a legend for a user who has not learned the keys yet, and a
/// user who has learned them asked to be rid of it. `launcher.show-hints`
/// reaches the renderer as a field on the frame, so this is the whole of what
/// the setting does on screen.
#[test]
fn the_footer_hides_its_hint_line_when_the_setting_says_so() {
    let context = create_launcher_context();
    let shown = build_launcher_frame(&context, launcher_input(Vec::new()), &model(""));
    let mut hidden_view = model("");
    hidden_view.show_hints = false;
    let hidden = build_launcher_frame(&context, launcher_input(Vec::new()), &hidden_view);
    for hint in ["Down", "Tab", "Escape", "navigate", "complete", "cancel"] {
        assert!(
            painted(&shown, hint),
            "default applied hint {hint} must be painted"
        );
        assert!(
            !painted(&hidden, hint),
            "disabled hint {hint} must not be painted"
        );
    }
}
/// The one thing hiding the hints must never hide.
///
/// `Settings` sits in the same footer row and is the only mouse route
/// into the settings surface: if it went with the hints, the setting that hid
/// them could only be undone from a terminal, by a user who no longer has
/// anything on screen telling them the surface exists.
#[test]
fn the_settings_control_survives_a_footer_with_no_hints() {
    let context = create_launcher_context();
    let mut view = model("");
    view.show_hints = false;

    let located = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&located, "Settings"));
    // And it is still the control, not just still painted: it answers a click
    // exactly as it does with the hints beside it.
    let affordance = position_of(&located, "Settings");
    let clicked = build_launcher_frame(
        &context,
        launcher_input(click_at(affordance + egui::vec2(4.0, 4.0))),
        &view,
    );
    assert!(clicked.commands.contains(&UiCommand::OpenSettings));

    // The same assertion with the hints shown, so a change that broke only one
    // of the two cases cannot pass by looking like the other.
    let with_hints = build_launcher_frame(&context, launcher_input(Vec::new()), &model(""));
    assert!(painted(&with_hints, "Settings"));
}

/// Set as text, the settings control has no button frame to say it can be
/// clicked, so the pointer has to be answered some other way or it reads as
/// part of the hint line beside it. It underlines under the pointer, and only
/// under the pointer.
#[test]
fn the_settings_text_answers_the_pointer_it_is_under() {
    fn underlines(frame: &crikey_ui::NativeUiFrame) -> usize {
        fn count(shape: &egui::Shape, found: &mut usize) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| count(shape, found)),
                // A horizontal hairline is the underline; the launcher draws
                // no other line segment, and a rule under the footer was
                // removed precisely because it read as a border.
                egui::Shape::LineSegment { points, .. } if points[0].y == points[1].y => *found += 1,
                _ => {}
            }
        }

        let mut found = 0;
        for clipped in &frame.output.shapes {
            count(&clipped.shape, &mut found);
        }
        found
    }

    let context = create_launcher_context();
    let view = model("");
    let located = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let settings = position_of(&located, "Settings");

    let elsewhere = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::PointerMoved(egui::pos2(8.0, 8.0))]),
        &view,
    );
    assert_eq!(
        underlines(&elsewhere),
        0,
        "a pointer away from the control must leave the footer as plain text"
    );

    let over = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::PointerMoved(settings + egui::vec2(4.0, 4.0))]),
        &view,
    );
    assert_eq!(
        underlines(&over),
        1,
        "text that opens a surface when clicked must show the pointer that it will"
    );
}

#[test]
fn the_settings_surface_lists_the_activation_hotkey_and_commits_an_edit() {
    let context = create_launcher_context();
    let view = hotkey_settings_model();

    let opened = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&opened, "launcher.activation-hotkey"));
    let editor = position_of(&opened, "Ctrl+Alt+Space");

    // Click into the editor, then type the new binding and commit it.
    let _ = build_launcher_frame(
        &context,
        launcher_input(click_at(editor + egui::vec2(4.0, 4.0))),
        &view,
    );
    let typed = vec![
        egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
        egui::Event::Text("!".to_owned()),
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
    ];
    force_three_layout_passes(&context, false);
    let committed = build_launcher_frame(&context, launcher_input(typed), &view);
    assert_eq!(committed.output.platform_output.num_completed_passes, 3);
    assert_eq!(
        committed
            .commands
            .iter()
            .filter(|command| matches!(command, UiCommand::SetSetting { .. }))
            .count(),
        1,
        "a first-pass settings commit is neither lost nor repeated"
    );

    let edit = committed
        .commands
        .iter()
        .find_map(|command| match command {
            UiCommand::SetSetting { key, value, .. } => Some((key.as_str(), value.as_str())),
            _ => None,
        })
        .expect("committing an edit must reach the host as SetSetting");
    assert_eq!(edit.0, "launcher.activation-hotkey");
    assert_ne!(
        edit.1, "Ctrl+Alt+Space",
        "the committed value must be what the user typed, not what was stored"
    );
}

#[test]
fn the_settings_surface_offers_a_quit_control() {
    let context = create_launcher_context();
    let view = hotkey_settings_model();

    let located = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let quit = position_of(&located, "Quit CriKey");

    let clicked = build_launcher_frame(
        &context,
        launcher_input(click_at(quit + egui::vec2(4.0, 4.0))),
        &view,
    );

    assert!(clicked.commands.contains(&UiCommand::Quit));
}

#[test]
fn plugin_details_and_index_refresh_are_reachable_background_requests() {
    use crikey_ui::settings_management::{Category, Package, Panel, Request};

    for kind in [
        None,
        Some(crikey_platform::PluginKind::Native),
        Some(crikey_platform::PluginKind::Legacy),
    ] {
        let context = create_launcher_context();
        context.style_mut(|style| style.animation_time = 0.0);
        let mut view = hotkey_settings_model();
        view.navigation.settings_management = Arc::new(Panel {
            schema: 1,
            category: Category::Plugins,
            packages: Arc::from([Package {
                id: "sample-package".into(),
                plugin: kind.map(|_| PluginId("operator.sample-package".into())),
                kind,
                name: "Sample package".into(),
                version: "1.0".into(),
                summary: "Package inventory entry".into(),
                installed: false,
                enabled: None,
            }]),
            ..Default::default()
        });
        let initial = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let (_, refresh) = reveal_settings_text(&context, &view, initial, "Update index", true);
        let refreshed = build_launcher_frame(
            &context,
            launcher_input(click_at(refresh + egui::vec2(4.0, 4.0))),
            &view,
        );
        assert!(refreshed
            .commands
            .contains(&UiCommand::ManageSettings(Request::UpdateIndex)));
        let (_, details) = reveal_settings_text(&context, &view, refreshed, "Package details", true);
        let detailed = build_launcher_frame(
            &context,
            launcher_input(click_at(details + egui::vec2(4.0, 4.0))),
            &view,
        );
        assert!(detailed
            .commands
            .contains(&UiCommand::ManageSettings(Request::Details(match kind {
                Some(kind) => crikey_ui::settings_management::PackageDetailsTarget::Local {
                    kind,
                    id: "sample-package".into()
                },
                None => crikey_ui::settings_management::PackageDetailsTarget::Index {
                    id: "sample-package".into()
                },
            }))));
    }
}

#[test]
fn visible_configure_command_opens_preferences_and_refocuses_on_a_new_visit() {
    use crikey_ui::settings_management::Category;
    let context = create_launcher_context();
    context.style_mut(|style| style.animation_time = 0.0);
    let reference = crikey_core::CommandRef {
        provider_id: PluginId("renderer-fixture".into()),
        command_id: "configure-command".into(),
    };
    let key = crikey_ui::command_edit_key(&reference, false);
    let mut command = result_row(0);
    command.target = crikey_ui::ActionTarget::Command(reference);
    command.source = None;
    command.command_state = Some(crikey_ui::CommandRowState {
        registry_revision: 4,
        preference_revision: 7,
        available: true,
        unavailable_reason: None,
        favorite: None,
        favorite_count: 0,
        hidden: false,
        fallback: None,
        fallback_count: 0,
        aliases: Vec::new(),
        hotkey: None,
        argument_policy: crikey_core::ArgumentPolicy::Forbidden,
        argument: None,
        ranking_reset_available: false,
    });
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.set_command_management_rows(vec![command.clone()]);
    let editor = egui::Id::new(("crikey-setting-draft", key.as_str())).with("editor");
    let mut previous_view = None;
    for generation in 1..=2 {
        // Leaving Settings retires the old root publication. Reopening actions
        // requires the fresh canonical publication requested by navigation.
        let generation = Generation::from_raw(generation);
        model.begin_generation(generation);
        model.publish(generation, vec![command.clone()], false);
        model.apply(UiCommand::ShowActions);
        let actions = model.frame().expect("command action sheet");
        let visible = build_launcher_frame(&context, launcher_input(Vec::new()), &actions);
        let configure = position_of(&visible, "Configure command…");
        assert!(
            visible.output.shapes.iter().any(|clipped| {
                text_origin(&clipped.shape, "Configure command…")
                    .is_some_and(|position| clipped.clip_rect.contains(position + egui::vec2(4.0, 4.0)))
            }),
            "configuration must be visible without scrolling past inline controls"
        );
        let clicked = build_launcher_frame(
            &context,
            launcher_input(click_at(configure + egui::vec2(4.0, 4.0))),
            &actions,
        );
        let intent = clicked
            .commands
            .into_iter()
            .find(|command| matches!(command, UiCommand::ActivateAction { .. }))
            .expect("clicking Configure emits the real prepared host action");
        assert!(
            model.apply(intent).is_none(),
            "configuration must not invoke the command"
        );
        assert_eq!(model.settings_focus(), Some(key.as_str()));
        let visit = model.view().expect("settings route").0;
        assert_ne!(previous_view, Some(visit));
        previous_view = Some(visit);
        let mut settings = model.frame().expect("settings presentation");
        let panel = Arc::make_mut(&mut settings.navigation.settings_management);
        panel.view = Some(visit);
        panel.category = Category::Shortcuts;
        let opened = build_launcher_frame(&context, launcher_input(Vec::new()), &settings);
        assert!(painted(&opened, "Command alias (separate from textual aliases)"));
        assert_eq!(context.memory(|memory| memory.focused()), Some(editor));
        context.memory_mut(|memory| memory.surrender_focus(editor));
        model.apply(UiCommand::CloseSettings);
        let root = model.frame().expect("restored launcher");
        let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &root);
        assert!(
            model.take_query_refresh().is_some(),
            "return requests a fresh root search"
        );
    }
}

#[test]
fn metadata_categories_select_fields_and_keep_history_clear_in_privacy() {
    use crikey_ui::settings_management::{Category, Field, Panel, Request};

    let context = create_launcher_context();
    context.style_mut(|style| style.animation_time = 0.0);
    let mut view = hotkey_settings_model();
    let field = |key: &str, label: &str, category: Category| Field {
        key: key.into(),
        label: label.into(),
        description: String::new(),
        category: category.label().into(),
        plugin: None,
        value: "12".into(),
        source: "user-global".into(),
        restart: false,
        secret: false,
        control: SettingControl::Text,
        max_bytes: 4096,
        value_available: true,
    };
    let mut panel = Panel {
        schema: 1,
        config: 1,
        projection_schema: 1,
        projection_config: 1,
        authoritative: true,
        fields: Arc::from([
            field("launcher.max-results", "Visible result limit", Category::Launcher),
            field(
                "launcher.rounded-corners",
                "Visible corner style",
                Category::Appearance,
            ),
        ]),
        ..Default::default()
    };
    view.navigation.settings_management = Arc::new(panel.clone());
    let initial = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let category = position_of(&initial, "Appearance");
    let selected = build_launcher_frame(
        &context,
        launcher_input(click_at(category + egui::vec2(4.0, 4.0))),
        &view,
    );
    assert!(selected
        .commands
        .contains(&UiCommand::ManageSettings(Request::Category(
            Category::Appearance
        ))));

    // Project the host-approved selection; filtering follows metadata rather
    // than deriving a category from the field's launcher-prefixed key.
    panel.category = Category::Appearance;
    view.navigation.settings_management = Arc::new(panel.clone());
    let appearance = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&appearance, "Visible corner style"));
    assert!(!painted(&appearance, "Visible result limit"));
    assert!(!painted(&appearance, "Clear query history"));
    assert!(!painted(&appearance, "Plugins / Background jobs"));

    panel.category = Category::PrivacyHistory;
    view.navigation.settings_management = Arc::new(panel);
    let privacy = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let (_, clear) = reveal_settings_text(&context, &view, privacy, "Clear query history", true);
    let cleared = build_launcher_frame(
        &context,
        launcher_input(click_at(clear + egui::vec2(4.0, 4.0))),
        &view,
    );
    assert!(cleared.commands.contains(&UiCommand::ClearQueryHistory));
}

#[test]
fn long_security_reports_wrap_without_moving_the_fixed_settings_quit_control() {
    use crikey_ui::settings_management::{Job, Panel};

    fn report_geometry(shape: &egui::Shape, report: &str) -> Option<(egui::Rect, usize)> {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == report => Some((
                egui::Rect::from_min_size(text.pos, text.galley.size()),
                text.galley.rows.len(),
            )),
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| report_geometry(shape, report)),
            _ => None,
        }
    }

    let report: Arc<str> = format!(
        "permissions={}\n{}full-security-information-END",
        "unbroken-owner-identity".repeat(180),
        "required capability: filesystem read/write; containment diagnostic retained\n".repeat(48),
    )
    .into();
    for width in [480.0, 1000.0] {
        let context = create_launcher_context();
        context.style_mut(|style| style.animation_time = 0.0);
        let mut view = hotkey_settings_model();
        view.navigation.settings_management = Arc::new(Panel {
            schema: 1,
            jobs: Arc::from([Job {
                id: 1,
                sequence: 1,
                operation: "doctor",
                status: Arc::clone(&report),
                terminal: true,
            }]),
            ..Default::default()
        });
        let input = |events| {
            let mut input = launcher_input(events);
            input.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 520.0),
            ));
            input
        };
        let mut current = build_launcher_frame(&context, input(Vec::new()), &view);
        let quit_anchor = position_of(&current, "Quit CriKey");
        let body_pointer = egui::pos2(width / 2.0, quit_anchor.y - 24.0);
        let mut package_header = None;
        for _ in 0..32 {
            package_header = current.output.shapes.iter().find_map(|shape| {
                text_origin(&shape.shape, "Plugins / Background jobs")
                    .map(|position| position + egui::vec2(4.0, 4.0))
                    .filter(|position| shape.clip_rect.contains(*position))
            });
            if package_header.is_some() {
                break;
            }
            current = build_launcher_frame(
                &context,
                input(vec![
                    egui::Event::PointerMoved(body_pointer),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -80.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ]),
                &view,
            );
        }
        build_launcher_frame(
            &context,
            input(click_at(
                package_header.expect("real jobs disclosure is reachable by body scrolling"),
            )),
            &view,
        );
        let mut expanded = build_launcher_frame(&context, input(Vec::new()), &view);
        for _ in 0..32 {
            if expanded
                .output
                .shapes
                .iter()
                .any(|shape| report_geometry(&shape.shape, &report).is_some())
            {
                break;
            }
            expanded = build_launcher_frame(
                &context,
                input(vec![
                    egui::Event::PointerMoved(body_pointer),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -80.0),
                        modifiers: egui::Modifiers::NONE,
                    },
                ]),
                &view,
            );
        }
        let (report_rect, rows) = expanded
            .output
            .shapes
            .iter()
            .find_map(|clipped| report_geometry(&clipped.shape, &report))
            .expect("the complete diagnostic is laid out, not elided or truncated");
        assert!(
            rows > report.lines().count(),
            "the unbroken identity must wrap as well as explicit lines"
        );
        assert!(
            report_rect.left() >= 0.0 && report_rect.right() <= width,
            "diagnostic exceeded the settings width: {report_rect:?}"
        );
        let quit = position_of(&expanded, "Quit CriKey");
        assert!(quit.x >= 0.0 && quit.x < width && quit.y >= 0.0 && quit.y < 520.0);
        let scrolled = build_launcher_frame(
            &context,
            input(vec![
                egui::Event::PointerMoved(body_pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -10000.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]),
            &view,
        );
        assert_eq!(
            position_of(&scrolled, "Quit CriKey"),
            quit,
            "only the settings body may scroll"
        );
        let clicked = build_launcher_frame(&context, input(click_at(quit + egui::vec2(4.0, 4.0))), &view);
        assert!(
            clicked.commands.contains(&UiCommand::Quit),
            "Quit must remain clickable after a long diagnostic at width {width}"
        );
    }
}

/// The window's corner and the query field's corner have to begin turning on
/// the same horizontal and vertical lines.
///
/// Two arcs a fixed distance apart do that exactly when the outer radius is
/// the inner radius plus that distance, which here is the panel margin. The
/// reported defect was both corners being rounded and the outer one still
/// looking wrong, so what this pins is the relationship, not the two numbers:
/// a later theme may round both more or less, but not independently.
#[test]
fn the_window_corner_is_concentric_with_the_query_field() {
    fn rounded_rects(shape: &egui::Shape, out: &mut Vec<egui::epaint::RectShape>) {
        match shape {
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| rounded_rects(shape, out)),
            egui::Shape::Rect(rect) => out.push(*rect),
            _ => {}
        }
    }

    let context = create_launcher_context();
    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &model(""));
    let mut rects = Vec::new();
    for clipped in &frame.output.shapes {
        rounded_rects(&clipped.shape, &mut rects);
    }

    // The canvas is the whole window and the field is the one sheet on it, so
    // widest-first is outer then inner without naming a colour.
    rects.sort_by(|left, right| right.rect.width().total_cmp(&left.rect.width()));
    let window = rects.first().expect("the launcher paints its canvas");
    let field = rects.get(1).expect("the launcher paints its query field");

    let gap = field.rect.min.x - window.rect.min.x;
    assert!(gap > 0.0, "the field must sit inside the window, not on its edge");
    assert_eq!(
        window.rounding.nw - field.rounding.nw,
        gap,
        "the window rounds by {} and the field by {} across a {gap} px margin, so their \
         arcs start on different lines: the outer corner cuts across the inner one",
        window.rounding.nw,
        field.rounding.nw
    );
    // A corner cannot be concentric on one side only.
    for radius in [window.rounding.ne, window.rounding.sw, window.rounding.se] {
        assert_eq!(
            radius, window.rounding.nw,
            "every window corner is the same corner"
        );
    }
}

/// Turning the corners off squares the silhouette the launcher paints, rather
/// than leaving a rounded one and hoping something clips it.
///
/// The window is undecorated, so this fill is the only edge there is. A
/// rounded fill with the setting off would leave the corners unpainted with
/// nothing behind them: transparent on a desktop that composites, and solid
/// black on one that does not.
#[test]
fn turning_the_corners_off_squares_what_the_launcher_paints() {
    fn widest_rect(frame: &crikey_ui::NativeUiFrame) -> egui::epaint::RectShape {
        fn rects(shape: &egui::Shape, out: &mut Vec<egui::epaint::RectShape>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| rects(shape, out)),
                egui::Shape::Rect(rect) => out.push(*rect),
                _ => {}
            }
        }

        let mut found = Vec::new();
        for clipped in &frame.output.shapes {
            rects(&clipped.shape, &mut found);
        }
        found.sort_by(|left, right| right.rect.width().total_cmp(&left.rect.width()));
        *found.first().expect("the launcher paints its canvas")
    }

    let context = create_launcher_context();
    let mut view = model("");

    let rounded = widest_rect(&build_launcher_frame(&context, launcher_input(Vec::new()), &view));
    assert!(
        rounded.rounding.nw > 0.0,
        "the default is a rounded window, or this tests nothing"
    );

    view.rounded_corners = false;
    let square = widest_rect(&build_launcher_frame(&context, launcher_input(Vec::new()), &view));
    for radius in [
        square.rounding.nw,
        square.rounding.ne,
        square.rounding.sw,
        square.rounding.se,
    ] {
        assert_eq!(radius, 0.0, "every corner is square once the setting says so");
    }
    assert_eq!(
        square.rect, rounded.rect,
        "the window keeps its size; only its corners change"
    );
}

/// A boolean setting is a switch, and moving it commits the opposite value
/// straight away.
///
/// Reported: a text field for a boolean is lame. It was also lossy -- the only
/// thing standing between a user and a rejected `yes` was that they happened
/// to type one of two words -- and a switch cannot produce a third answer.
#[test]
fn a_boolean_setting_is_a_switch_that_commits_when_it_moves() {
    fn boolean_model(on: bool) -> ViewModel {
        let mut view = model("");
        view.settings_open = true;
        view.settings = Arc::from(vec![SettingRow {
            key: "launcher.show-hints".to_owned(),
            label: "Show keyboard hints".to_owned(),
            value: on.to_string(),
            source: "built-in-defaults".to_owned(),
            control: SettingControl::Toggle { on },
        }]);
        view
    }

    let context = create_launcher_context();
    let view = boolean_model(true);
    let drawn = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    // A button's label is a galley of exactly that word. `painted` is a
    // substring search and the sheet's own footer reads "Enter or Save commits
    // an edit", which is about the text rows and matches either way.
    fn labelled_exactly(frame: &crikey_ui::NativeUiFrame, label: &str) -> bool {
        fn walk(shape: &egui::Shape, label: &str) -> bool {
            match shape {
                egui::Shape::Text(text) => text.galley.text() == label,
                egui::Shape::Vec(shapes) => shapes.iter().any(|shape| walk(shape, label)),
                _ => false,
            }
        }

        frame
            .output
            .shapes
            .iter()
            .any(|clipped| walk(&clipped.shape, label))
    }

    assert!(painted(&drawn, "Show keyboard hints"), "the label still names it");
    assert!(
        !labelled_exactly(&drawn, "Save"),
        "a switch commits when it moves, so a Save beside it would say the click had not counted"
    );

    fn switch_position(frame: &crikey_ui::NativeUiFrame) -> egui::Pos2 {
        fn tracks(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Rect(rect)
                    if (rect.rect.width() - 2.0 * rect.rect.height()).abs() < 0.1
                        && (rect.rounding.nw - rect.rect.height() / 2.0).abs() < 0.1
                        && rect.fill != egui::Color32::TRANSPARENT =>
                {
                    out.push(rect.rect)
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        tracks(shape, out);
                    }
                }
                _ => {}
            }
        }
        let mut rendered = Vec::new();
        for shape in &frame.output.shapes {
            tracks(&shape.shape, &mut rendered);
        }
        assert_eq!(rendered.len(), 1, "the actual switch track is unique");
        rendered[0].center()
    }
    let switch = switch_position(&drawn);
    let clicked = build_launcher_frame(&context, launcher_input(click_at(switch)), &view);

    let committed = clicked
        .commands
        .iter()
        .find_map(|command| match command {
            UiCommand::SetSetting { key, value, .. } => Some((key.as_str(), value.as_str())),
            _ => None,
        })
        .expect("moving the switch commits the setting");
    assert_eq!(
        committed,
        ("launcher.show-hints", "false"),
        "an on switch turns off"
    );

    // And back, from the other position.
    let off = boolean_model(false);
    let drawn_off = build_launcher_frame(&context, launcher_input(Vec::new()), &off);
    let switch_off = switch_position(&drawn_off);
    let clicked_off = build_launcher_frame(&context, launcher_input(click_at(switch_off)), &off);
    let committed_off = clicked_off
        .commands
        .iter()
        .find_map(|command| match command {
            UiCommand::SetSetting { key, value, .. } => Some((key.as_str(), value.as_str())),
            _ => None,
        })
        .expect("moving the switch back commits the setting");
    assert_eq!(
        committed_off,
        ("launcher.show-hints", "true"),
        "an off switch turns on"
    );
}

/// A switch has to look like a control in *both* positions.
///
/// The stock checkbox did not: its off state was an empty rounded square a
/// shade from the surface behind it, which reads as nothing being there at
/// all. So this asserts the two states differ in what they paint, not merely
/// that a click commits -- a switch nobody can see is a switch nobody uses,
/// and that is the defect the shipped screenshot showed.
#[test]
fn a_switch_is_visible_in_both_positions() {
    /// Every filled shape's colour, which is the switch's whole appearance:
    /// a track and a knob.
    fn fills(frame: &crikey_ui::NativeUiFrame) -> Vec<egui::Color32> {
        fn walk(shape: &egui::Shape, out: &mut Vec<egui::Color32>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, out)),
                egui::Shape::Rect(rect) => out.push(rect.fill),
                egui::Shape::Circle(circle) => out.push(circle.fill),
                _ => {}
            }
        }

        let mut found = Vec::new();
        for clipped in &frame.output.shapes {
            walk(&clipped.shape, &mut found);
        }
        found
    }

    fn sheet(on: bool) -> ViewModel {
        let mut view = model("");
        view.settings_open = true;
        view.settings = Arc::from(vec![SettingRow {
            key: "launcher.show-hints".to_owned(),
            label: "Show keyboard hints".to_owned(),
            value: on.to_string(),
            source: "built-in-defaults".to_owned(),
            control: SettingControl::Toggle { on },
        }]);
        view
    }

    let context = create_launcher_context();
    let on = fills(&build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &sheet(true),
    ));
    let off = fills(&build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &sheet(false),
    ));

    assert_ne!(
        on, off,
        "the two positions of a switch must not paint the same thing"
    );

    // The knob is the only circle this sheet paints, which is what makes it
    // findable without reaching into the renderer. Asserting on the switch's
    // own shapes rather than on every fill in the frame: the query field and
    // the buttons stand out from the sheet whatever the switch does, so a
    // frame-wide search would have passed with the invisible checkbox still
    // in place.
    for (state, view) in [("on", sheet(true)), ("off", sheet(false))] {
        let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let (knob, track) = switch_shapes(&frame);
        assert_ne!(
            knob, track,
            "the {state} switch's knob is the colour of its own track, so there is no knob"
        );
        assert!(
            !sheet_colours().contains(&track),
            "the {state} switch's track is the colour of the sheet, so there is no switch"
        );
    }
}

/// The two surface tiers a settings row is drawn on: the sheet it stands on
/// and the window under that.
///
/// Spelled out rather than read from the theme, because a test that took them
/// from the theme would agree with the renderer by construction and prove
/// nothing about contrast. Their being written here is also why they have to
/// be right: an earlier guess at the sheet's colour let a switch painted in
/// exactly that colour pass, which is the whole defect.
fn sheet_colours() -> [egui::Color32; 2] {
    [
        // the default appearance surface
        egui::Color32::from_rgb(34, 38, 45),
        // the default appearance canvas
        egui::Color32::from_rgb(20, 22, 26),
    ]
}

/// The switch's knob and track colours: the sheet's only circle, and the
/// rectangle under it.
fn switch_shapes(frame: &crikey_ui::NativeUiFrame) -> (egui::Color32, egui::Color32) {
    fn walk(
        shape: &egui::Shape,
        knob: &mut Option<(egui::Pos2, egui::Color32)>,
        rects: &mut Vec<egui::epaint::RectShape>,
    ) {
        match shape {
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, knob, rects)),
            egui::Shape::Circle(circle) => *knob = Some((circle.center, circle.fill)),
            egui::Shape::Rect(rect) => rects.push(*rect),
            _ => {}
        }
    }

    let mut knob = None;
    let mut rects = Vec::new();
    for clipped in &frame.output.shapes {
        walk(&clipped.shape, &mut knob, &mut rects);
    }
    let (centre, colour) = knob.expect("a switch paints a knob");
    let track = rects
        .iter()
        .filter(|rect| rect.rect.contains(centre))
        // The innermost rectangle containing the knob is its track; the sheet
        // and the window contain it too.
        .min_by(|left, right| left.rect.area().total_cmp(&right.rect.area()))
        .expect("the knob sits on a track");
    (colour, track.fill)
}

// ---------------------------------------------------------------------------
// Plugin-drawn pages (spec 32)
//
// Every assertion here is on the frame the shipped renderer builds, because
// the whole claim of a page is that the *host* draws it: a plugin hands over a
// display list and semantics, and what ends up on screen has to be the
// launcher's own shapes inside the launcher's own rectangle.
// ---------------------------------------------------------------------------

/// A colour nothing else in the launcher paints, used by the probe node below.
const PROBE: PageColor = PageColor::rgba(1, 2, 3, 255);

/// A one-pixel node every page fixture draws at the page's own origin.
///
/// The page's position is a layout result, and a test that recomputed it from
/// the theme's margins would agree with the renderer by construction and prove
/// nothing about where nodes actually land. Measuring it from a node the plugin
/// drew at (0, 0) is the only way to assert that a page-relative coordinate is
/// page-relative.
fn probe_node() -> PageNode {
    PageNode {
        shape: NodeShape::Rect,
        width: 1.0,
        height: 1.0,
        fill: PROBE,
        ..PageNode::default()
    }
}

fn page_node(shape: NodeShape, x: f32, y: f32, width: f32, height: f32) -> PageNode {
    PageNode {
        shape,
        x,
        y,
        width,
        height,
        ..PageNode::default()
    }
}

/// A focusable control: an interactive role and a node id are both required,
/// which is what `PageNode::is_focusable` says.
fn page_control(role: NodeRole, node_id: u32, x: f32, y: f32, width: f32, height: f32) -> PageNode {
    PageNode {
        role,
        node_id,
        ..page_node(NodeShape::Rect, x, y, width, height)
    }
}

fn page_view(nodes: Vec<PageNode>) -> ViewModel {
    page_view_at_generation(nodes, 1)
}

/// A page whose frame carries `generation`, for the tests that care that the
/// host keeps advancing it while the picture inside the page does not change.
fn page_view_at_generation(nodes: Vec<PageNode>, generation: u64) -> ViewModel {
    let mut view = model("");
    let mut all = vec![probe_node()];
    all.extend(nodes);
    let frame = PageFrame {
        generation,
        title: "Demo Page".to_owned(),
        nodes: all,
        ..PageFrame::default()
    };
    frame
        .validate()
        .expect("a page fixture must be a frame the host would have accepted");
    view.page = Some(PageSurface {
        plugin: PluginId("demo".to_owned()),
        page_id: "page-1".to_owned(),
        plugin_name: "Demo Plugin".to_owned(),
        frame: Arc::new(frame),
        stale: false,
        answered: true,
        web_surface_id: None,
        web_frame: None,
        web_generation: 0,
        web_caret_area: None,
    });
    view
}

/// Every rectangle the frame paints, with the clip rectangle it was painted
/// under.
///
/// The clip travels with the shape because that is how egui expresses
/// clipping: a node outside the page is still tessellated, and what keeps it
/// off the launcher's chrome is the rectangle it is drawn under.
fn rect_shapes(frame: &crikey_ui::NativeUiFrame) -> Vec<(egui::epaint::RectShape, egui::Rect)> {
    fn walk(shape: &egui::Shape, clip: egui::Rect, found: &mut Vec<(egui::epaint::RectShape, egui::Rect)>) {
        match shape {
            egui::Shape::Rect(rect) => found.push((*rect, clip)),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    walk(shape, clip, found);
                }
            }
            _ => {}
        }
    }

    let mut found = Vec::new();
    for clipped in &frame.output.shapes {
        walk(&clipped.shape, clipped.clip_rect, &mut found);
    }
    found
}

/// A page surface whose plugin has, or has not, ever answered.
///
/// Unlike [`page_view`] this adds no probe node, because the cases under test
/// include the frame that carries nothing at all -- both the placeholder the
/// host puts up on open, and the empty page a plugin may publish on purpose.
fn lifecycle_page_view(nodes: Vec<PageNode>, stale: bool, answered: bool) -> ViewModel {
    let mut view = model("");
    let frame = PageFrame {
        generation: 1,
        title: "Demo Page".to_owned(),
        nodes,
        ..PageFrame::default()
    };
    frame
        .validate()
        .expect("a page fixture must be a frame the host would have accepted");
    view.page = Some(PageSurface {
        plugin: PluginId("demo".to_owned()),
        page_id: "page-1".to_owned(),
        plugin_name: "Demo Plugin".to_owned(),
        frame: Arc::new(frame),
        stale,
        answered,
        web_surface_id: None,
        web_frame: None,
        web_generation: 0,
        web_caret_area: None,
    });
    view
}

/// How many stroked open paths the frame paints, which is what an
/// `egui::Spinner` is made of.
fn spinner_arcs(frame: &crikey_ui::NativeUiFrame) -> usize {
    fn walk(shape: &egui::Shape, found: &mut usize) {
        match shape {
            egui::Shape::Path(path) if !path.closed => *found += 1,
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    walk(shape, found);
                }
            }
            _ => {}
        }
    }

    let mut found = 0;
    for clipped in &frame.output.shapes {
        walk(&clipped.shape, &mut found);
    }
    found
}

#[track_caller]
fn page_origin(frame: &crikey_ui::NativeUiFrame) -> egui::Pos2 {
    rect_shapes(frame)
        .into_iter()
        .find(|(rect, _)| rect.fill == egui::Color32::from_rgb(PROBE.r, PROBE.g, PROBE.b))
        .map(|(rect, _)| rect.rect.min)
        .expect("the probe node the fixture draws at the page origin must be painted")
}

fn page_inputs(frame: &crikey_ui::NativeUiFrame) -> Vec<PageInput> {
    frame
        .commands
        .iter()
        .filter_map(|command| match command {
            UiCommand::PageInput(input) => Some(input.clone()),
            _ => None,
        })
        .collect()
}

fn events_of_kind(frame: &crikey_ui::NativeUiFrame, kind: PageInputKind) -> Vec<PageInput> {
    page_inputs(frame)
        .into_iter()
        .filter(|input| input.kind == kind)
        .collect()
}

fn key_press(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

#[test]
fn a_page_rect_node_is_painted_in_the_colour_the_plugin_asked_for() {
    let context = create_launcher_context();
    let mut node = page_node(NodeShape::Rect, 10.0, 20.0, 80.0, 24.0);
    node.fill = PageColor::rgba(200, 30, 40, 255);
    let view = page_view(vec![node]);

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    let origin = page_origin(&frame);
    let expected = egui::Rect::from_min_size(origin + egui::vec2(10.0, 20.0), egui::vec2(80.0, 24.0));
    assert!(
        rect_shapes(&frame)
            .iter()
            .any(|(rect, _)| rect.rect == expected && rect.fill == egui::Color32::from_rgb(200, 30, 40)),
        "a Rect node must be painted at its own coordinates, offset by the page origin, in its own fill"
    );
}

#[test]
fn a_page_text_node_paints_its_text() {
    let context = create_launcher_context();
    let mut node = page_node(NodeShape::Text, 8.0, 8.0, 200.0, 20.0);
    node.text = "Nothing here but us nodes".to_owned();
    node.fill = PageColor::rgba(240, 240, 240, 255);
    let view = page_view(vec![node]);

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    assert!(
        painted(&frame, "Nothing here but us nodes"),
        "a Text node's string must reach the frame"
    );
}

/// A rounding larger than the rectangle it rounds must be clamped, not obeyed.
///
/// egui tessellates a corner arc of whatever radius it is handed, so a radius
/// past half the shorter side sends the two arcs of one side through each
/// other: a wide flat button asking for a pill by naming a huge radius comes
/// out pinched in the middle instead. Half the shorter side is the largest
/// radius that still fits.
#[test]
fn an_over_large_page_rounding_is_clamped_instead_of_inverting_the_rect() {
    let context = create_launcher_context();
    let mut node = page_node(NodeShape::Rect, 0.0, 40.0, 120.0, 30.0);
    node.fill = PageColor::rgba(90, 140, 220, 255);
    node.rounding = 500.0;
    let view = page_view(vec![node]);

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    let (shape, _) = rect_shapes(&frame)
        .into_iter()
        .find(|(rect, _)| rect.fill == egui::Color32::from_rgb(90, 140, 220))
        .expect("the node is painted");
    assert_eq!(
        shape.rounding.nw, 15.0,
        "the rounding must be clamped to half the shorter side (30 / 2), not taken literally"
    );
}

/// A node whose coordinates leave the page must not paint over the launcher.
///
/// Node coordinates are the plugin's, so a negative `y` is a thing a plugin
/// will produce -- and the launcher's query field is directly above the page,
/// so an unclipped one would draw over the text the user is typing. The node
/// below sits far enough above the page's top edge to be entirely inside the
/// query field's band.
#[test]
fn a_page_node_outside_the_page_is_clipped_away_from_the_launcher_chrome() {
    let context = create_launcher_context();
    let mut node = page_node(NodeShape::Rect, 0.0, -40.0, 200.0, 20.0);
    node.fill = PageColor::rgba(255, 0, 255, 255);
    let view = page_view(vec![node]);

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    let (shape, clip) = rect_shapes(&frame)
        .into_iter()
        .find(|(rect, _)| rect.fill == egui::Color32::from_rgb(255, 0, 255))
        .expect("the node is still tessellated; the clip is what keeps it off the screen");
    assert!(
        !clip.intersects(shape.rect),
        "a node above the page must be clipped entirely away, not drawn over the query field"
    );
}

/// Alpha zero is how the vocabulary spells a hit target with no appearance.
///
/// So it must paint nothing and still answer the pointer. Refusing it instead
/// would take away the only way a page can put an invisible control over
/// something it drew itself.
#[test]
fn a_zero_alpha_page_fill_paints_nothing_and_still_hit_tests() {
    let context = create_launcher_context();
    let invisible = page_control(NodeRole::Button, 1, 0.0, 0.0, 100.0, 30.0);
    // A second control, ahead of the first in the ring, so the host's focus
    // outline is drawn somewhere else and cannot be mistaken for the invisible
    // node painting itself.
    let mut visible = page_control(NodeRole::Button, 2, 0.0, 120.0, 100.0, 30.0);
    visible.fill = PageColor::rgba(60, 60, 60, 255);
    visible.focus_order = 0;
    let mut invisible = invisible;
    invisible.focus_order = 1;
    let view = page_view(vec![invisible, visible]);

    let origin = {
        let probe = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        page_origin(&probe)
    };
    let hidden = egui::Rect::from_min_size(origin, egui::vec2(100.0, 30.0));
    let frame = build_launcher_frame(&context, launcher_input(click_at(hidden.center())), &view);

    // Nothing the *plugin* asked for is painted here: a zero-alpha fill is a
    // hit target, not a shape. The host's own focus outline is a different
    // matter and is expected -- clicking a control focuses it, and a user who
    // cannot see what they just focused has been told nothing -- so the
    // assertion is about fills rather than about rectangles.
    let painted: Vec<_> = rect_shapes(&frame)
        .into_iter()
        .filter(|(shape, _)| shape.rect == hidden)
        .collect();
    assert!(
        painted.iter().all(|(shape, _)| shape.fill.a() == 0),
        "an invisible fill must paint no filled rectangle"
    );
    assert!(
        painted.iter().any(|(shape, _)| shape.stroke.width > 0.0),
        "the focus the click moved must still be visible to the user"
    );
    let pressed = events_of_kind(&frame, PageInputKind::PointerPressed);
    assert_eq!(
        pressed.iter().map(|input| input.node_id).collect::<Vec<_>>(),
        vec![1],
        "the invisible node must still be the node the pointer reports"
    );
}

/// Tab walks `PageFrame::focus_ring`, which is the plugin's `focus_order` and
/// then document order -- never the order the nodes happen to be listed in.
///
/// The fixture lists the nodes in the opposite order to their focus order, so
/// a renderer that walked the display list instead would produce a different
/// sequence on the very first frame.
#[test]
fn tab_walks_the_page_focus_ring_in_ring_order_and_wraps() {
    let context = create_launcher_context();
    let mut first = page_control(NodeRole::Button, 10, 0.0, 0.0, 60.0, 20.0);
    first.focus_order = 2;
    let mut second = page_control(NodeRole::Button, 20, 0.0, 30.0, 60.0, 20.0);
    second.focus_order = 0;
    let mut third = page_control(NodeRole::Button, 30, 0.0, 60.0, 60.0, 20.0);
    third.focus_order = 1;
    let mut launcher = page_launcher(page_view(vec![first, second, third]));
    let view = launcher.frame().unwrap();
    let mut generation = 1;

    let mut reached = Vec::new();
    for events in [
        Vec::new(),
        vec![key_press(egui::Key::Tab, egui::Modifiers::NONE)],
        vec![key_press(egui::Key::Tab, egui::Modifiers::NONE)],
        vec![key_press(egui::Key::Tab, egui::Modifiers::NONE)],
        vec![key_press(egui::Key::Tab, egui::Modifiers::SHIFT)],
    ] {
        let frame = build_launcher_frame(&context, launcher_input(events), &view);
        let mut effects = Vec::new();
        for command in frame.commands {
            apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
        }
        reached.extend(effects.iter().filter_map(|effect| match effect {
            crikey_ui::UiEffect::PageInput(input) if input.kind == PageInputKind::FocusChanged => {
                Some(input.node_id)
            }
            _ => None,
        }));
        assert!(
            !effects.iter().any(|effect| matches!(effect,
                crikey_ui::UiEffect::PageInput(input) if input.kind == PageInputKind::KeyPressed
            )),
            "display-list Tab traverses host focus without forwarding a plugin key"
        );
    }

    assert_eq!(
        reached,
        vec![20, 30, 10, 20, 10],
        "focus starts at the head of the ring, walks it in ring order, wraps, and Shift+Tab \
         walks back"
    );
}

/// Escape closes the page and is never forwarded.
///
/// It is the host's one reserved key: a plugin that could see it could swallow
/// it, and a user inside a surface with no way out is a launcher that has to be
/// killed from a terminal.
#[test]
fn escape_closes_the_page_and_never_reaches_the_plugin() {
    let context = create_launcher_context();
    let mut launcher = page_launcher(page_view(vec![page_control(
        NodeRole::Button,
        1,
        0.0,
        0.0,
        60.0,
        20.0,
    )]));
    let (returned, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![key_press(egui::Key::Escape, egui::Modifiers::NONE)],
    );
    assert!(returned.page.is_none());
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, crikey_ui::UiEffect::ClosePage))
            .count(),
        1
    );
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, crikey_ui::UiEffect::PageInput(input) if input.key == "Escape")));
}

#[test]
fn a_page_pointer_press_reports_the_node_under_it_in_page_coordinates() {
    let context = create_launcher_context();
    let view = page_view(vec![page_control(NodeRole::Button, 7, 20.0, 30.0, 60.0, 20.0)]);

    let origin = {
        let probe = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        page_origin(&probe)
    };
    let frame = build_launcher_frame(
        &context,
        launcher_input(click_at(origin + egui::vec2(35.0, 40.0))),
        &view,
    );

    let pressed = events_of_kind(&frame, PageInputKind::PointerPressed);
    let [pressed] = pressed.as_slice() else {
        panic!("one press must be reported, got {pressed:?}");
    };
    assert_eq!(pressed.node_id, 7, "the press must name the node under it");
    assert_eq!(
        (pressed.x, pressed.y),
        (35.0, 40.0),
        "the coordinates must be the page's own, not the window's"
    );
    assert!(
        events_of_kind(&frame, PageInputKind::Activated)
            .iter()
            .any(|input| input.node_id == 7),
        "a click on a control is an activation as well as a press"
    );
}

/// Typed text reaches a field and nothing else.
///
/// A keystroke that lands nowhere is better than one silently editing a field
/// the user cannot see, so text is delivered only while the focused node is a
/// `TextField` and is dropped for every other role.
#[test]
fn typed_text_reaches_a_page_text_field_and_is_swallowed_by_a_button() {
    fn typed(role: NodeRole) -> Vec<PageInput> {
        let context = create_launcher_context();
        let view = page_view(vec![page_control(role, 1, 0.0, 0.0, 120.0, 24.0)]);
        // Two frames: the first puts the host's focus on the node, the second
        // types into it. A single frame would be testing a keystroke that
        // arrived before anything was focused.
        let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let frame = build_launcher_frame(
            &context,
            launcher_input(vec![egui::Event::Text("q".to_owned())]),
            &view,
        );
        events_of_kind(&frame, PageInputKind::TextInput)
    }

    let into_field = typed(NodeRole::TextField);
    let [into_field] = into_field.as_slice() else {
        panic!("typing into a focused field must reach the plugin, got {into_field:?}");
    };
    assert_eq!(into_field.text, "q");
    assert_eq!(into_field.node_id, 1);

    assert!(
        typed(NodeRole::Button).is_empty(),
        "text typed while a button holds the keyboard has nowhere to land and must be dropped"
    );
}

#[test]
fn display_list_input_and_initial_focus_survive_layout_passes_exactly_once() {
    let context = create_launcher_context();
    let view = page_view(vec![page_control(NodeRole::TextField, 1, 0.0, 0.0, 120.0, 24.0)]);
    force_three_layout_passes(&context, true);
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            egui::Event::Text("a".to_owned()),
            egui::Event::Text("a".to_owned()),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
        ]),
        &view,
    );
    assert_eq!(frame.output.platform_output.num_completed_passes, 3);
    let focus = events_of_kind(&frame, PageInputKind::FocusChanged);
    assert_eq!(focus.len(), 1, "first-pass focus must reach the plugin once");
    assert_eq!(focus[0].node_id, 1);
    let text = events_of_kind(&frame, PageInputKind::TextInput);
    assert_eq!(text.len(), 2);
    assert!(text.iter().all(|event| event.node_id == 1 && event.text == "a"));
    assert_eq!(events_of_kind(&frame, PageInputKind::KeyPressed).len(), 3);
    assert_eq!(
        frame
            .commands
            .iter()
            .filter(|command| matches!(command, UiCommand::ResizePage { .. }))
            .count(),
        1,
        "only the final measured display-list viewport is published"
    );
}

/// The three key spellings the protocol documents by example.
///
/// The host's spelling is what a plugin matches on, so it is part of the wire
/// contract rather than an implementation detail: `Key::name` would have sent
/// `Down` where the protocol says `ArrowDown`, and a plugin waiting for the
/// documented word would never have fired.
#[test]
fn page_keys_are_spelled_the_way_the_protocol_documents() {
    let context = create_launcher_context();
    let view = page_view(vec![page_control(NodeRole::Button, 1, 0.0, 0.0, 60.0, 20.0)]);

    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            key_press(egui::Key::A, egui::Modifiers::SHIFT),
        ]),
        &view,
    );

    let keys: Vec<String> = events_of_kind(&frame, PageInputKind::KeyPressed)
        .into_iter()
        .map(|input| input.key)
        .collect();
    assert_eq!(keys, vec!["Enter", "ArrowDown", "A"]);
}

/// The footer must stay inside the window while a page is open.
///
/// The page area is measured from what is left after the footer and the gap
/// above it, exactly as the result list is. A surface that took everything
/// available would push the status line past the bottom edge -- and the status
/// line is the only thing telling the user whose surface they are looking at,
/// which makes losing it worse here than in a result list.
#[test]
fn an_open_page_still_leaves_the_footer_inside_the_window() {
    let context = create_launcher_context();
    // A page taller than the window, which is what a plugin drawing a long
    // form produces: the page is clipped, the footer is not moved.
    let mut tall = page_node(NodeShape::Rect, 0.0, 0.0, 400.0, 900.0);
    tall.fill = PageColor::rgba(40, 44, 52, 255);
    let view = page_view(vec![tall]);

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    let window = NativeLauncherConfig::default();
    let footer = position_of(&frame, "Demo Page");
    assert!(
        footer.y + crikey_ui::egui::TextStyle::Small.resolve(&context.style()).size <= window.height as f32,
        "the page's title line is the footer, and it must be drawn inside the window: found at \
         {footer:?} in a {}px window",
        window.height
    );
    assert!(
        painted(&frame, "Demo Plugin"),
        "the footer must name the plugin whose surface is showing"
    );
}

/// A page waiting on its plugin has to look different from one that is up to
/// date, because the nodes on screen are the previous answer and the user
/// cannot otherwise tell.
#[test]
fn a_stale_page_says_so_in_the_status_line() {
    let context = create_launcher_context();
    let mut view = page_view(vec![page_node(NodeShape::Rect, 0.0, 0.0, 40.0, 40.0)]);

    let fresh = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(!painted(&fresh, "Waiting for the plugin"));

    view.page.as_mut().expect("the fixture opens a page").stale = true;
    let stale = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(
        painted(&stale, "Waiting for the plugin"),
        "a stale page must be visibly distinguishable from a fresh one"
    );
}

// ---------------------------------------------------------------------------
// Page rasters (spec 32)
//
// The one node whose content the host does not compose: the plugin hands over
// finished pixels, so what is under test is the upload, its reuse and its
// release rather than any drawing decision.
// ---------------------------------------------------------------------------

/// The extent of every raster fixture below.
///
/// Deliberately unlike the icon slot and unlike any font atlas page, so a
/// frame's texture deltas can be filtered down to the page's own picture by
/// size alone.
const RASTER: [usize; 2] = [3, 2];

/// A solid raster. Opaque, because egui stores premultiplied colour and an
/// opaque pixel is the one case where the uploaded bytes must equal the ones
/// the plugin sent.
fn raster(colour: [u8; 4]) -> PageImage {
    PageImage {
        pixel_width: RASTER[0] as u32,
        pixel_height: RASTER[1] as u32,
        rgba: colour
            .iter()
            .copied()
            .cycle()
            .take(RASTER[0] * RASTER[1] * 4)
            .collect(),
    }
}

/// An image node carrying `colour`, with a fill the host must ignore.
fn image_node(x: f32, y: f32, width: f32, height: f32, colour: [u8; 4]) -> PageNode {
    let mut node = page_node(NodeShape::Image, x, y, width, height);
    node.image = Some(raster(colour));
    // A fill no raster may ever be multiplied by: the plugin already decided
    // what its pixels look like.
    node.fill = PageColor::rgba(255, 0, 0, 255);
    node
}

/// How many of this frame's texture uploads are page rasters.
fn raster_uploads(frame: &crikey_ui::NativeUiFrame) -> usize {
    frame
        .output
        .textures_delta
        .set
        .iter()
        .filter(|(_, delta)| delta.image.size() == RASTER)
        .count()
}

#[track_caller]
fn raster_texture(frame: &crikey_ui::NativeUiFrame) -> egui::TextureId {
    frame
        .output
        .textures_delta
        .set
        .iter()
        .find(|(_, delta)| delta.image.size() == RASTER)
        .map(|(id, _)| *id)
        .expect("the page's raster is uploaded as a texture of its own pixel extent")
}

#[test]
fn a_page_image_node_paints_its_raster_into_its_own_rectangle_untinted() {
    let context = create_launcher_context();
    let colour = [10, 200, 90, 255];
    let view = page_view(vec![image_node(12.0, 16.0, 60.0, 40.0, colour)]);

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    let id = raster_texture(&frame);
    let (_, delta) = frame
        .output
        .textures_delta
        .set
        .iter()
        .find(|(uploaded, _)| *uploaded == id)
        .expect("the raster just found is in this frame's deltas");
    match &delta.image {
        egui::ImageData::Color(image) => assert_eq!(
            image.pixels[0],
            egui::Color32::from_rgba_premultiplied(colour[0], colour[1], colour[2], colour[3]),
            "the host uploads the plugin's pixels unaltered"
        ),
        egui::ImageData::Font(_) => panic!("a raster uploads colour data, not font coverage"),
    }

    let origin = page_origin(&frame);
    let expected = egui::Rect::from_min_size(origin + egui::vec2(12.0, 16.0), egui::vec2(60.0, 40.0));
    let (shape, clip) = rect_shapes(&frame)
        .into_iter()
        .find(|(shape, _)| shape.fill_texture_id == id)
        .expect("the uploaded raster is painted in this frame");
    assert_eq!(
        shape.rect, expected,
        "a raster is drawn into the node's own rectangle, offset by the page origin"
    );
    assert!(
        clip.contains_rect(expected),
        "the raster must be drawn under the page's clip, like every other node: {clip:?} does not \
         contain {expected:?}"
    );
    assert_eq!(
        shape.fill,
        egui::Color32::WHITE,
        "a raster must be drawn untinted: the node's fill colours the shapes the host composes, \
         not the pixels the plugin composed itself"
    );
}

/// One upload for a raster the plugin keeps drawing.
///
/// This is the memoisation contract, and the reason it is a contract rather
/// than an optimisation: a page rebuilds its whole display list every frame
/// and is redrawn on every input event, so a renderer that uploaded per frame
/// would push the raster across the bus on every keystroke. The second frame
/// is built from a *separately constructed* page whose pixels merely match, so
/// nothing here can pass by reusing the first frame's allocation.
#[test]
fn a_raster_drawn_on_two_consecutive_frames_is_uploaded_once() {
    let context = create_launcher_context();
    let colour = [10, 200, 90, 255];

    let first = build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &page_view(vec![image_node(0.0, 8.0, 30.0, 20.0, colour)]),
    );
    // A later generation, because that is what a real second frame carries:
    // the host advances it on every input and resize. Keying the cache on
    // anything that moves with the frame - the generation above all - would
    // miss on every keystroke while still passing a two-identical-frames
    // test, so the fixture deliberately does not hold it still.
    let second = build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &page_view_at_generation(vec![image_node(0.0, 8.0, 30.0, 20.0, colour)], 2),
    );

    assert_eq!(raster_uploads(&first), 1, "the first frame uploads the raster");
    assert_eq!(
        raster_uploads(&second),
        0,
        "a page redrawing the same picture must reuse the texture it already holds"
    );
    assert!(
        rect_shapes(&second)
            .iter()
            .any(|(shape, _)| shape.fill_texture_id == raster_texture(&first)),
        "the reused texture is what the second frame paints"
    );
}

/// A closed page's rasters are freed, because nothing will ever draw them
/// again. A launcher that kept them would hold a megabyte of GPU memory for
/// every page the user opened during the session.
#[test]
fn closing_a_page_releases_the_rasters_it_uploaded() {
    let context = create_launcher_context();
    let view = page_view(vec![image_node(0.0, 8.0, 30.0, 20.0, [10, 200, 90, 255])]);

    let opened = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let id = raster_texture(&opened);

    let closed = build_launcher_frame(&context, launcher_input(Vec::new()), &model(""));

    assert!(
        closed.output.textures_delta.free.contains(&id),
        "the texture the page uploaded must be freed when the page closes"
    );
}

/// Opening a page can cost hundreds of milliseconds before its plugin
/// produces a first frame, and the sheet is blank for all of it. The footer
/// says why, but the user who just pressed Enter is looking at the sheet, so
/// a bare panel reads as a hang.
///
/// Counted rather than located: a waiting page already puts one spinner in
/// the footer, so the assertion is that the sheet adds a second one, and the
/// cases below pin that it adds nothing at any other time.
#[test]
fn a_page_that_has_never_been_answered_says_so_on_the_sheet() {
    let context = create_launcher_context();
    let opening = lifecycle_page_view(Vec::new(), true, false);
    let answered = lifecycle_page_view(vec![probe_node()], true, true);

    let opening = build_launcher_frame(&context, launcher_input(Vec::new()), &opening);
    let answered = build_launcher_frame(&context, launcher_input(Vec::new()), &answered);

    assert_eq!(
        spinner_arcs(&opening),
        spinner_arcs(&answered) + 1,
        "a page that has never been drawn must say it is loading on the sheet, not only in \
         the footer"
    );
}

/// The case that makes the marker necessary rather than a convenience. A
/// plugin may publish a page with no nodes on purpose -- a cleared canvas, a
/// form that has just been submitted -- and while the host waits for its next
/// frame that page is both empty and stale, which is exactly the shape of a
/// page that has never answered. Covering it with a spinner would tell the
/// user their finished page is still loading.
#[test]
fn an_empty_page_the_plugin_published_is_not_treated_as_loading() {
    let context = create_launcher_context();
    let published = lifecycle_page_view(Vec::new(), true, true);

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &published);

    assert_eq!(
        spinner_arcs(&frame),
        1,
        "an empty page that was answered is finished, not loading: only the footer's spinner \
         may show"
    );
}

/// And the reason this cannot key on `stale` alone: a page that redraws on a
/// timer is stale between every frame, and a spinner over the picture it is
/// already showing would blink on every one of them.
#[test]
fn a_page_already_showing_a_frame_is_not_covered_by_a_spinner() {
    let context = create_launcher_context();
    let settled = lifecycle_page_view(vec![probe_node()], false, true);
    let stale = lifecycle_page_view(vec![probe_node()], true, true);

    let settled = build_launcher_frame(&context, launcher_input(Vec::new()), &settled);
    let stale = build_launcher_frame(&context, launcher_input(Vec::new()), &stale);

    assert_eq!(
        spinner_arcs(&settled),
        0,
        "a settled page must show no spinner at all"
    );
    assert_eq!(
        spinner_arcs(&stale),
        1,
        "a stale page keeps the frame it has: the footer is the only thing that says so"
    );
}

/// A web surface's plugin answers the instant it declares a URL, but the
/// engine behind it took 276 to 533 ms to produce a first frame when
/// measured. Judging the loading state on the plugin's answer would clear the
/// spinner immediately and leave the user watching a blank sheet for that
/// whole interval, which is precisely the hang the spinner exists to cover.
#[test]
fn a_declared_web_surface_keeps_loading_until_its_first_frame_is_painted() {
    let context = create_launcher_context();

    let mut declared = lifecycle_page_view(Vec::new(), false, true);
    let surface = declared.page.as_mut().expect("the fixture opens a page");
    surface.frame = Arc::new(crikey_core::PageFrame {
        generation: 1,
        title: "Demo Page".to_owned(),
        web: Some(crikey_core::PageWebSurface {
            url: "https://example.invalid/".to_owned(),
            ..crikey_core::PageWebSurface::default()
        }),
        ..crikey_core::PageFrame::default()
    });

    let waiting = build_launcher_frame(&context, launcher_input(Vec::new()), &declared);
    assert_eq!(
        spinner_arcs(&waiting),
        1,
        "a web surface that has answered but never painted must still say it is loading"
    );

    let surface = declared.page.as_mut().expect("the fixture opens a page");
    surface.web_surface_id = Some(41);
    surface.web_generation = 1;
    surface.web_frame = Some(Arc::new(WebSurfaceFrame {
        surface_id: 41,
        generation: 1,
        pixel_width: 2,
        pixel_height: 1,
        rgba8: Arc::from([20, 40, 60, 255, 80, 100, 120, 255]),
    }));
    let painted = build_launcher_frame(&context, launcher_input(Vec::new()), &declared);
    assert_eq!(
        spinner_arcs(&painted),
        0,
        "once the engine has painted a frame the sheet has content and the spinner goes"
    );

    let surface = declared.page.as_mut().expect("the fixture opens a page");
    surface.web_frame = None;
    let evicted = build_launcher_frame(&context, launcher_input(Vec::new()), &declared);
    assert_eq!(
        spinner_arcs(&evicted),
        0,
        "evicting pixels after a painted generation must not regress to first-frame loading"
    );
}

fn painted_web_view(surface_id: u64, generation: u64, rgba8: &[u8]) -> ViewModel {
    let mut view = lifecycle_page_view(Vec::new(), false, true);
    let page = view.page.as_mut().expect("the fixture opens a page");
    page.frame = Arc::new(crikey_core::PageFrame {
        generation: 1,
        title: "Web Page".to_owned(),
        web: Some(crikey_core::PageWebSurface {
            url: "https://example.invalid/".to_owned(),
            ..crikey_core::PageWebSurface::default()
        }),
        ..crikey_core::PageFrame::default()
    });
    page.web_surface_id = Some(surface_id);
    page.web_generation = generation;
    page.web_frame = Some(Arc::new(WebSurfaceFrame {
        surface_id,
        generation,
        pixel_width: 2,
        pixel_height: 1,
        rgba8: Arc::from(rgba8),
    }));
    view
}

fn page_launcher(mut fixture: ViewModel) -> LauncherViewModel {
    let mut launcher = action_launcher();
    // A second selectable row makes a leaked post-close Down observable.
    launcher.publish(Generation::from_raw(1), vec![result_row(0), result_row(1)], false);
    assert!(launcher.open_page(fixture.page.take().expect("a real page surface")));
    launcher
}

#[test]
fn a_web_generation_uploads_exact_premultiplied_rgba_and_paints_it_untinted() {
    let context = create_launcher_context();
    let bytes = [10, 20, 30, 40, 90, 100, 110, 120];
    let view = painted_web_view(41, 1, &bytes);

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let (texture, delta) = frame
        .output
        .textures_delta
        .set
        .iter()
        .find(|(_, delta)| delta.image.size() == [2, 1])
        .expect("the complete web raster must be uploaded at its exact extent");
    match &delta.image {
        egui::ImageData::Color(image) => assert_eq!(
            image.pixels.as_slice(),
            &[
                egui::Color32::from_rgba_premultiplied(10, 20, 30, 40),
                egui::Color32::from_rgba_premultiplied(90, 100, 110, 120),
            ],
            "premultiplied engine bytes must reach the texture without another alpha multiplication"
        ),
        egui::ImageData::Font(_) => panic!("a web frame uploads colour data, not font coverage"),
    }

    let (shape, clip) = rect_shapes(&frame)
        .into_iter()
        .find(|(shape, _)| shape.fill_texture_id == *texture)
        .expect("the uploaded web texture must be painted in the same frame");
    assert!(shape.rect.width() > 0.0 && shape.rect.height() > 0.0);
    assert!(
        clip.contains_rect(shape.rect),
        "the document is clipped to its page"
    );
    assert_eq!(
        shape.fill,
        egui::Color32::WHITE,
        "the engine already composed the frame, so the launcher must not tint it"
    );
}

#[test]
fn web_textures_are_reused_per_generation_and_evicted_on_replacement_and_close() {
    let context = create_launcher_context();
    let first_view = painted_web_view(41, 1, &[1, 2, 3, 4, 5, 6, 7, 8]);
    let first = build_launcher_frame(&context, launcher_input(Vec::new()), &first_view);
    let first_id = first
        .output
        .textures_delta
        .set
        .iter()
        .find(|(_, delta)| delta.image.size() == [2, 1])
        .map(|(id, _)| *id)
        .expect("the first generation uploads");

    let repeated = build_launcher_frame(&context, launcher_input(Vec::new()), &first_view);
    assert!(
        repeated
            .output
            .textures_delta
            .set
            .iter()
            .all(|(_, delta)| delta.image.size() != [2, 1]),
        "a repaint of one generation must reuse its texture"
    );
    assert!(
        rect_shapes(&repeated)
            .iter()
            .any(|(shape, _)| shape.fill_texture_id == first_id),
        "the reused texture remains the one painted"
    );

    let second_view = painted_web_view(41, 2, &[8, 7, 6, 5, 4, 3, 2, 1]);
    let second = build_launcher_frame(&context, launcher_input(Vec::new()), &second_view);
    let second_id = second
        .output
        .textures_delta
        .set
        .iter()
        .find(|(_, delta)| delta.image.size() == [2, 1])
        .map(|(id, _)| *id)
        .expect("a new generation uploads its replacement");
    assert_ne!(second_id, first_id);
    assert!(
        second.output.textures_delta.free.contains(&first_id),
        "replacing a generation frees its previous GPU texture"
    );

    let closed = build_launcher_frame(&context, launcher_input(Vec::new()), &model(""));
    assert!(
        closed.output.textures_delta.free.contains(&second_id),
        "closing the web page frees its final GPU texture"
    );
}

fn web_document_rect(frame: &crikey_ui::NativeUiFrame) -> egui::Rect {
    rect_shapes(frame)
        .into_iter()
        .find(|(shape, _)| shape.fill_texture_id != egui::TextureId::default())
        .expect("the fixture paints its web texture")
        .0
        .rect
}

fn web_inputs(frame: &crikey_ui::NativeUiFrame) -> Vec<WebSurfaceInput> {
    frame
        .commands
        .iter()
        .filter_map(|command| match command {
            UiCommand::WebInput(input) => Some(input.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn equal_web_events_remain_ordered_and_are_not_replayed_by_layout_passes() {
    for change_geometry in [false, true] {
        let context = create_launcher_context();
        let view = painted_web_view(41, 1, &[10; 8]);
        force_three_layout_passes(&context, change_geometry);
        // egui marks subsequent presses of a held key as repeats. The last two
        // presses must remain equal without collapsing into one command.
        let press = key_press(egui::Key::ArrowDown, egui::Modifiers::NONE);
        let mut input = launcher_input(vec![
            egui::Event::Text("a".to_owned()),
            egui::Event::Text("a".to_owned()),
            press.clone(),
            press.clone(),
            press,
            egui::Event::Ime(egui::ImeEvent::Commit("b".to_owned())),
        ]);
        input.time = Some(12.0);
        let frame = build_launcher_frame(&context, input, &view);
        assert_eq!(frame.output.platform_output.num_completed_passes, 3);
        let rect = web_document_rect(&frame);
        let resize = UiCommand::ResizePage {
            width: rect.width().round() as u32,
            height: rect.height().round() as u32,
        };
        let commit = UiCommand::WebIme(WebImeEvent::Commit {
            surface_id: 41,
            text: "a".to_owned(),
        });
        let key = UiCommand::WebInput(WebSurfaceInput::Key {
            surface_id: 41,
            timestamp_ms: 12_000,
            pressed: true,
            keysym: 0xff54,
            hardware_keycode: 0,
            repeat: false,
            modifiers: 0,
        });
        let mut repeated_key = key.clone();
        if let UiCommand::WebInput(WebSurfaceInput::Key { repeat, .. }) = &mut repeated_key {
            *repeat = true;
        }
        assert_eq!(
            frame.commands,
            vec![
                resize,
                commit.clone(),
                commit,
                key,
                repeated_key.clone(),
                repeated_key,
                UiCommand::WebIme(WebImeEvent::Commit {
                    surface_id: 41,
                    text: "b".to_owned(),
                }),
            ],
            "only the final viewport replaces first-pass work; equal real input stays intact"
        );
        let repaint = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        assert!(web_inputs(&repaint).is_empty());
        assert!(!repaint
            .commands
            .iter()
            .any(|command| matches!(command, UiCommand::WebIme(_))));
    }
}

#[test]
fn leaving_the_window_clears_document_hover_without_an_intermediate_outside_motion() {
    let context = create_launcher_context();
    let view = painted_web_view(41, 1, &[10; 8]);
    let first = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let inside = web_document_rect(&first).center();
    let moved = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::PointerMoved(inside)]),
        &view,
    );
    assert_eq!(web_inputs(&moved).len(), 1);
    force_three_layout_passes(&context, false);
    let departed = build_launcher_frame(&context, launcher_input(vec![egui::Event::PointerGone]), &view);
    assert_eq!(departed.output.platform_output.num_completed_passes, 3);
    assert!(matches!(
        web_inputs(&departed).as_slice(),
        [WebSurfaceInput::PointerMove { surface_id: 41, x, y, .. }] if *x < 0.0 && *y < 0.0
    ));
    let repeated = build_launcher_frame(&context, launcher_input(vec![egui::Event::PointerGone]), &view);
    assert!(web_inputs(&repeated).is_empty(), "departure is reported once");
}

#[test]
fn a_window_departure_keeps_the_captured_release_outside_without_an_outside_motion() {
    for one_frame in [false, true] {
        let context = create_launcher_context();
        let view = painted_web_view(41, 1, &[10; 8]);
        let first = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let inside = web_document_rect(&first).center();
        let mut events = vec![
            egui::Event::PointerMoved(inside),
            egui::Event::PointerButton {
                pos: inside,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        if !one_frame {
            let pressed = build_launcher_frame(&context, launcher_input(events), &view);
            assert!(matches!(
                web_inputs(&pressed).last(),
                Some(WebSurfaceInput::PointerButton { pressed: true, .. })
            ));
            events = Vec::new();
        }
        events.extend([
            egui::Event::PointerGone,
            egui::Event::PointerButton {
                // A release with no new motion can still carry the old inside
                // coordinate. It cannot turn the departed drag into a click.
                pos: inside,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        let released = build_launcher_frame(&context, launcher_input(events), &view);
        let inputs = web_inputs(&released);
        assert_eq!(inputs.len(), if one_frame { 4 } else { 2 });
        assert!(
            matches!(
                &inputs[inputs.len() - 2..],
                [
                    WebSurfaceInput::PointerMove { surface_id: 41, x: move_x, y: move_y, .. },
                    WebSurfaceInput::PointerButton {
                        surface_id: 41,
                        button: 1,
                        pressed: false,
                        x: release_x,
                        y: release_y,
                        ..
                    },
                ] if *move_x == -1.0 && *move_y == -1.0
                    && *release_x == -1.0 && *release_y == -1.0
            ),
            "departure and release must both be outside the original control"
        );
        let after = build_launcher_frame(
            &context,
            launcher_input(vec![
                egui::Event::PointerButton {
                    pos: inside,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, 1.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]),
            &view,
        );
        assert!(
            web_inputs(&after).is_empty(),
            "release ends capture despite stale inside coordinates"
        );
        let reentered = build_launcher_frame(
            &context,
            launcher_input(vec![egui::Event::PointerMoved(inside)]),
            &view,
        );
        assert!(
            matches!(
                web_inputs(&reentered).as_slice(),
                [WebSurfaceInput::PointerMove { x, y, .. }] if *x >= 0.0 && *y >= 0.0
            ),
            "real motion restores document hover after re-entry"
        );
    }
}

#[test]
fn web_composition_enables_ime_at_the_page_caret_without_editing_the_old_query() {
    let context = create_launcher_context();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &model("unchanged"));
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &model("unchanged"));
    let mut view = painted_web_view(41, 1, &[10; 8]);
    view.page.as_mut().expect("web page").web_caret_area = Some((4.0, 5.0, 2.0, 12.0));
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            egui::Event::Ime(egui::ImeEvent::Enabled),
            egui::Event::Ime(egui::ImeEvent::Preedit("éλ".to_owned())),
            egui::Event::Ime(egui::ImeEvent::Commit("é".to_owned())),
        ]),
        &view,
    );
    let rect = web_document_rect(&frame);
    let caret = egui::Rect::from_min_size(rect.min + egui::vec2(4.0, 5.0), egui::vec2(2.0, 12.0));
    let ime = frame
        .output
        .platform_output
        .ime
        .expect("the document keeps the native IME enabled");
    assert_eq!(ime.rect, caret);
    assert_eq!(ime.cursor_rect, caret);
    let ime_commands: Vec<_> = frame
        .commands
        .iter()
        .filter_map(|command| match command {
            UiCommand::WebIme(event) => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(
        ime_commands,
        vec![
            &WebImeEvent::Preedit {
                surface_id: 41,
                text: "éλ".to_owned(),
                cursor: None
            },
            &WebImeEvent::Commit {
                surface_id: 41,
                text: "é".to_owned()
            },
        ]
    );
    assert!(!frame
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetQuery(_))));
    let display_list = build_launcher_frame(&context, launcher_input(Vec::new()), &page_view(Vec::new()));
    assert!(
        display_list.output.platform_output.ime.is_none(),
        "the previous engine's caret is retired"
    );
}

#[test]
fn web_scrolls_use_event_order_page_units_and_page_local_coordinates() {
    let context = create_launcher_context();
    let view = painted_web_view(41, 1, &[10; 8]);
    let first = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let rect = web_document_rect(&first);
    let inside = rect.min + egui::vec2(11.0, 12.0);
    let outside = rect.max + egui::vec2(5.0, 5.0);
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            egui::Event::PointerMoved(inside),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(2.0, 3.0),
                modifiers: egui::Modifiers::SHIFT,
            },
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Page,
                delta: egui::vec2(1.0, -1.0),
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerMoved(outside),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, 1.0),
                modifiers: egui::Modifiers::NONE,
            },
        ]),
        &view,
    );
    let inputs = web_inputs(&frame);
    let scrolls: Vec<_> = inputs
        .iter()
        .filter(|input| matches!(input, WebSurfaceInput::Scroll { .. }))
        .collect();
    assert_eq!(
        scrolls.len(),
        2,
        "the later wheel event aimed at chrome is not document input"
    );
    assert!(matches!(
        scrolls[0],
        WebSurfaceInput::Scroll {
            x: 11.0,
            y: 12.0,
            delta_x: 2.0,
            delta_y: 3.0,
            precise: true,
            modifiers: WEB_MODIFIER_SHIFT,
            ..
        }
    ));
    assert!(
        matches!(scrolls[1], WebSurfaceInput::Scroll { x: 11.0, y: 12.0, delta_x, delta_y, precise: true, .. } if *delta_x == rect.width() && *delta_y == -rect.height())
    );
    assert!(inputs.iter().any(|input| matches!(input, WebSurfaceInput::PointerMove { x, y, .. } if *x > rect.width() && *y > rect.height())), "leaving the document clears engine hover");
}

#[test]
fn a_web_drag_keeps_chorded_buttons_and_releases_outside_the_document() {
    let context = create_launcher_context();
    let view = painted_web_view(41, 1, &[10; 8]);
    let first = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let rect = web_document_rect(&first);
    let inside = rect.min + egui::vec2(11.0, 12.0);
    let outside = rect.max + egui::vec2(5.0, 6.0);
    let press = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::PointerButton {
            pos: inside,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]),
        &view,
    );
    assert!(web_inputs(&press).iter().any(|input| matches!(
        input,
        WebSurfaceInput::PointerButton {
            button: 1,
            pressed: true,
            ..
        }
    )));
    let release = build_launcher_frame(
        &context,
        launcher_input(vec![
            egui::Event::PointerGone,
            egui::Event::PointerMoved(outside),
            egui::Event::PointerButton {
                pos: outside,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: outside,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: outside,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, 1.0),
                modifiers: egui::Modifiers::NONE,
            },
        ]),
        &view,
    );
    let inputs = web_inputs(&release);
    let buttons: Vec<_> = inputs
        .iter()
        .filter_map(|input| match input {
            WebSurfaceInput::PointerButton {
                button,
                pressed,
                x,
                y,
                ..
            } => Some((*button, *pressed, *x, *y)),
            _ => None,
        })
        .collect();
    assert_eq!(
        buttons,
        vec![
            (3, true, rect.width() + 5.0, rect.height() + 6.0),
            (1, false, rect.width() + 5.0, rect.height() + 6.0),
            (3, false, rect.width() + 5.0, rect.height() + 6.0),
        ]
    );
    assert!(
        !inputs
            .iter()
            .any(|input| matches!(input, WebSurfaceInput::Scroll { .. })),
        "capture ends after every held button is released"
    );
}

#[test]
fn replacing_a_web_surface_retires_pointer_capture_and_the_previous_texture() {
    let context = create_launcher_context();
    let first_view = painted_web_view(41, 1, &[10; 8]);
    let first = build_launcher_frame(&context, launcher_input(Vec::new()), &first_view);
    let rect = web_document_rect(&first);
    let old_texture = rect_shapes(&first)
        .into_iter()
        .find(|(shape, _)| shape.rect == rect && shape.fill_texture_id != egui::TextureId::default())
        .expect("web texture")
        .0
        .fill_texture_id;
    let _ = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::PointerButton {
            pos: rect.center(),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]),
        &first_view,
    );
    let replacement = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::PointerButton {
            pos: rect.max + egui::vec2(5.0, 5.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]),
        &painted_web_view(42, 1, &[20; 8]),
    );
    assert!(
        web_inputs(&replacement).is_empty(),
        "the new document cannot receive the old document's drag release"
    );
    assert!(replacement.output.textures_delta.free.contains(&old_texture));
    assert!(
        replacement
            .output
            .textures_delta
            .set
            .iter()
            .any(|(_, delta)| delta.image.size() == [2, 1]),
        "surface identity, not generation alone, decides texture reuse"
    );
}

#[test]
fn web_escape_preserves_earlier_input_and_retires_the_rest_of_the_batch() {
    for multiple_passes in [false, true] {
        let context = create_launcher_context();
        if multiple_passes {
            force_three_layout_passes(&context, false);
        }
        let mut launcher = page_launcher(painted_web_view(41, 1, &[10; 8]));
        let (returned, effects) = drive_ordered_action_input(
            &context,
            &mut launcher,
            vec![
                egui::Event::Text("before".to_owned()),
                key_press(egui::Key::Escape, egui::Modifiers::CTRL),
                egui::Event::Text("after".to_owned()),
                key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            ],
        );
        let document: Vec<_> = effects
            .iter()
            .filter(|effect| {
                matches!(
                    effect,
                    crikey_ui::UiEffect::WebInput(_)
                        | crikey_ui::UiEffect::WebIme(_)
                        | crikey_ui::UiEffect::ClosePage
                )
            })
            .cloned()
            .collect();
        assert_eq!(
            document,
            vec![
                crikey_ui::UiEffect::WebIme(WebImeEvent::Commit {
                    surface_id: 41,
                    text: "before".into()
                }),
                crikey_ui::UiEffect::ClosePage,
            ]
        );
        assert!(returned.page.is_none());
        assert_eq!(returned.query, "fixture");
        assert_eq!(returned.selected, 0);
        assert!(!returned.settings_open);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, crikey_ui::UiEffect::Query(query) if query != "fixture")));
    }
}

#[test]
fn an_unadmitted_web_declaration_still_answers_host_escape() {
    let context = create_launcher_context();
    let mut view = painted_web_view(41, 1, &[10; 8]);
    let page = view.page.as_mut().expect("page");
    page.web_surface_id = None;
    page.web_frame = None;
    let mut launcher = page_launcher(view);
    let (returned, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            egui::Event::Text("retired".into()),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
        ],
    );
    assert!(returned.page.is_none());
    assert_eq!(returned.query, "fixture");
    assert_eq!(returned.selected, 0);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, crikey_ui::UiEffect::ClosePage))
            .count(),
        1
    );
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        crikey_ui::UiEffect::WebInput(_) | crikey_ui::UiEffect::WebIme(_)
    )));
}

#[test]
fn the_first_web_wheel_uses_the_previous_position_not_a_later_motion() {
    for prior_chrome_position in [false, true] {
        let context = create_launcher_context();
        let view = painted_web_view(41, 1, &[10; 8]);
        // Another context measures geometry without priming this context's
        // document pointer state.
        let measured = build_launcher_frame(&create_launcher_context(), launcher_input(Vec::new()), &view);
        let inside = web_document_rect(&measured).center();
        if prior_chrome_position {
            let _ = build_launcher_frame(
                &context,
                launcher_input(vec![egui::Event::PointerMoved(egui::pos2(1.0, 1.0))]),
                &model(""),
            );
        }
        force_three_layout_passes(&context, false);
        let frame = build_launcher_frame(
            &context,
            launcher_input(vec![
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, 1.0),
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerMoved(inside),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, 2.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]),
            &view,
        );
        let scrolls: Vec<_> = web_inputs(&frame)
            .into_iter()
            .filter(|input| matches!(input, WebSurfaceInput::Scroll { .. }))
            .collect();
        assert!(
            matches!(scrolls.as_slice(), [WebSurfaceInput::Scroll { delta_y: 2.0, .. }]),
            "the first wheel was aimed at chrome or had no known pointer position: {scrolls:?}"
        );
    }
}

#[test]
fn web_named_keys_and_text_reach_the_engine_but_escape_and_chrome_shortcuts_do_not_leak() {
    for multiple_passes in [false, true] {
        let context = create_launcher_context();
        if multiple_passes {
            force_three_layout_passes(&context, false);
        }
        let mut launcher = page_launcher(painted_web_view(41, 1, &[10; 8]));
        let mut tab_release = key_press(egui::Key::Tab, egui::Modifiers::SHIFT);
        if let egui::Event::Key { pressed, .. } = &mut tab_release {
            *pressed = false;
        }
        let (returned, effects) = drive_ordered_action_input(
            &context,
            &mut launcher,
            vec![
                key_press(egui::Key::Tab, egui::Modifiers::SHIFT),
                tab_release,
                egui::Event::Text("É".to_owned()),
                key_press(egui::Key::Escape, egui::Modifiers::CTRL | egui::Modifiers::SHIFT),
                key_press(egui::Key::Comma, egui::Modifiers::COMMAND),
            ],
        );
        let document: Vec<_> = effects
            .iter()
            .filter(|effect| {
                matches!(
                    effect,
                    crikey_ui::UiEffect::WebInput(_)
                        | crikey_ui::UiEffect::WebIme(_)
                        | crikey_ui::UiEffect::ClosePage
                )
            })
            .collect();
        assert_eq!(document.len(), 4);
        assert!(matches!(
            document[0],
            crikey_ui::UiEffect::WebInput(WebSurfaceInput::Key {
                surface_id: 41,
                keysym: 0xff09,
                pressed: true,
                modifiers: WEB_MODIFIER_SHIFT,
                ..
            })
        ));
        assert!(matches!(
            document[1],
            crikey_ui::UiEffect::WebInput(WebSurfaceInput::Key {
                surface_id: 41,
                keysym: 0xff09,
                pressed: false,
                modifiers: WEB_MODIFIER_SHIFT,
                ..
            })
        ));
        assert!(
            matches!(document[2], crikey_ui::UiEffect::WebIme(WebImeEvent::Commit {
            surface_id: 41, text
        }) if text == "É")
        );
        assert_eq!(document[3], &crikey_ui::UiEffect::ClosePage);
        assert!(returned.page.is_none());
        assert!(!returned.settings_open);
        assert_eq!(returned.query, "fixture");
        assert_eq!(returned.selected, 0);
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            crikey_ui::UiEffect::PageInput(_) | crikey_ui::UiEffect::ManageSettings(_)
        )));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, crikey_ui::UiEffect::Query(query) if query != "fixture")));
    }
}

#[test]
fn a_web_document_keeps_the_footer_settings_route_without_leaking_chrome_clicks() {
    for show_hints in [false, true] {
        let context = create_launcher_context();
        let mut view = painted_web_view(41, 1, &[10; 8]);
        view.show_hints = show_hints;
        let located = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let settings = position_of(&located, "Settings") + egui::vec2(4.0, 4.0);
        let inside = web_document_rect(&located).center();
        let _ = build_launcher_frame(
            &context,
            launcher_input(vec![egui::Event::PointerMoved(inside)]),
            &view,
        );
        force_three_layout_passes(&context, false);
        let clicked = build_launcher_frame(&context, launcher_input(click_at(settings)), &view);
        assert_eq!(
            clicked
                .commands
                .iter()
                .filter(|command| matches!(command, UiCommand::OpenSettings))
                .count(),
            1,
            "host settings must remain reachable with or without page hints"
        );
        assert!(
            !web_inputs(&clicked)
                .iter()
                .any(|input| matches!(input, WebSurfaceInput::PointerButton { .. })),
            "the footer click belongs to launcher chrome, not the document"
        );
    }
}

#[test]
fn web_keyboard_navigation_cannot_activate_the_host_settings_control() {
    for admitted in [false, true] {
        for retained_settings_focus in [false, true] {
            for key in [egui::Key::Enter, egui::Key::Space] {
                let context = create_launcher_context();
                let mut view = painted_web_view(41, 1, &[10; 8]);
                if !admitted {
                    let page = view.page.as_mut().expect("pending declaration");
                    page.web_surface_id = None;
                    page.web_frame = None;
                    page.web_generation = 0;
                }
                // An empty display-list page has no document widgets; Tab
                // therefore gives the host Settings control keyboard focus.
                // Carry that real focus across a rendering-model transition.
                let previous = if retained_settings_focus {
                    page_view(Vec::new())
                } else {
                    view.clone()
                };
                let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &previous);
                let tab = build_launcher_frame(
                    &context,
                    launcher_input(vec![key_press(egui::Key::Tab, egui::Modifiers::NONE)]),
                    &previous,
                );
                if retained_settings_focus {
                    assert!(
                        context.memory(|memory| memory.focused()).is_some(),
                        "the previous host control must really have keyboard focus"
                    );
                } else if admitted {
                    assert!(matches!(
                        web_inputs(&tab).as_slice(),
                        [WebSurfaceInput::Key {
                            keysym: 0xff09,
                            pressed: true,
                            ..
                        }]
                    ));
                }
                let mut events = vec![key_press(key, egui::Modifiers::NONE)];
                if key == egui::Key::Space {
                    events.push(egui::Event::Text(" ".to_owned()));
                }
                force_three_layout_passes(&context, false);
                let entered = build_launcher_frame(&context, launcher_input(events), &view);
                assert_eq!(entered.output.platform_output.num_completed_passes, 3);
                if admitted && key == egui::Key::Enter {
                    assert!(matches!(
                        web_inputs(&entered).as_slice(),
                        [WebSurfaceInput::Key {
                            keysym: 0xff0d,
                            pressed: true,
                            ..
                        }]
                    ));
                } else if admitted {
                    assert!(entered.commands.contains(&UiCommand::WebIme(WebImeEvent::Commit {
                        surface_id: 41,
                        text: " ".to_owned(),
                    })));
                } else {
                    assert!(web_inputs(&entered).is_empty());
                    assert!(!entered
                        .commands
                        .iter()
                        .any(|command| matches!(command, UiCommand::WebIme(_))));
                }
                assert!(
                    !entered.commands.iter().any(|command| matches!(
                        command,
                        UiCommand::OpenSettings | UiCommand::SetQuery(_) | UiCommand::Quit
                    )),
                    "web-owned keys cannot activate host chrome, even before admission or after host focus"
                );
            }
        }
    }
}

#[test]
fn oversized_web_textures_are_dropped_before_upload_and_release_the_old_texture() {
    let context = create_launcher_context();
    let mut view = painted_web_view(41, 1, &[10; 8]);
    let first = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let rect = web_document_rect(&first);
    let old_texture = rect_shapes(&first)
        .into_iter()
        .find(|(shape, _)| shape.rect == rect && shape.fill_texture_id != egui::TextureId::default())
        .expect("web texture")
        .0
        .fill_texture_id;
    view.page.as_mut().expect("web page").web_frame = Some(Arc::new(WebSurfaceFrame {
        surface_id: 41,
        generation: 2,
        pixel_width: 2049,
        pixel_height: 1,
        rgba8: vec![255; 2049 * 4].into(),
    }));
    let mut input = launcher_input(Vec::new());
    input.max_texture_side = Some(2048);
    let frame = build_launcher_frame(&context, input, &view);
    assert!(frame.output.textures_delta.free.contains(&old_texture));
    assert!(!frame
        .output
        .textures_delta
        .set
        .iter()
        .any(|(_, delta)| delta.image.size() == [2049, 1]));
    assert!(!rect_shapes(&frame)
        .iter()
        .any(|(shape, _)| shape.fill_texture_id == old_texture));
}

#[test]
fn every_replacement_web_engine_receives_the_measured_viewport_once() {
    let context = create_launcher_context();
    let first_view = painted_web_view(41, 1, &[10; 8]);
    let first = build_launcher_frame(&context, launcher_input(Vec::new()), &first_view);
    let expected: Vec<_> = first
        .commands
        .iter()
        .filter_map(|command| match command {
            UiCommand::ResizePage { width, height } => Some((*width, *height)),
            _ => None,
        })
        .collect();
    assert_eq!(expected.len(), 1);
    let repeated = build_launcher_frame(&context, launcher_input(Vec::new()), &first_view);
    assert!(!repeated
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::ResizePage { .. })));
    let replacement_view = painted_web_view(42, 1, &[20; 8]);
    let replacement = build_launcher_frame(&context, launcher_input(Vec::new()), &replacement_view);
    let replacement_sizes: Vec<_> = replacement
        .commands
        .iter()
        .filter_map(|command| match command {
            UiCommand::ResizePage { width, height } => Some((*width, *height)),
            _ => None,
        })
        .collect();
    assert_eq!(
        replacement_sizes, expected,
        "the new host must not remain at its initial guessed viewport"
    );
    let mut resized_input = launcher_input(Vec::new());
    resized_input.screen_rect = resized_input
        .screen_rect
        .map(|rect| egui::Rect::from_min_size(rect.min, rect.size() + egui::vec2(100.0, 60.0)));
    let resized = build_launcher_frame(&context, resized_input, &replacement_view);
    assert!(resized.commands.iter().any(|command| matches!(command, UiCommand::ResizePage { width, height } if *width > expected[0].0 && *height > expected[0].1)));
}

#[test]
fn returning_from_web_to_a_display_list_releases_only_the_engine_texture() {
    let context = create_launcher_context();
    let web = build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &painted_web_view(41, 1, &[10; 8]),
    );
    let web_texture = rect_shapes(&web)
        .into_iter()
        .find(|(shape, _)| shape.fill_texture_id != egui::TextureId::default())
        .expect("web texture")
        .0
        .fill_texture_id;
    let view = page_view(vec![image_node(0.0, 8.0, 30.0, 20.0, [10, 200, 90, 255])]);
    let display_list = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(display_list.output.textures_delta.free.contains(&web_texture));
    assert_eq!(raster_uploads(&display_list), 1);
    let raster = raster_texture(&display_list);
    assert!(rect_shapes(&display_list)
        .iter()
        .any(|(shape, _)| shape.fill_texture_id == raster));
    assert!(!display_list
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::WebInput(_) | UiCommand::WebIme(_))));
}

#[test]
fn admission_uses_pointer_motion_from_pending_frames_for_the_first_stationary_wheel() {
    for had_previous_surface in [false, true] {
        for enters_document in [false, true] {
            let context = create_launcher_context();
            let previous_view = painted_web_view(41, 1, &[10; 8]);
            let measured = build_launcher_frame(
                &create_launcher_context(),
                launcher_input(Vec::new()),
                &previous_view,
            );
            let inside = web_document_rect(&measured).center();
            let chrome = egui::pos2(1.0, 1.0);
            let (before, pending_position) = if enters_document {
                (chrome, inside)
            } else {
                (inside, chrome)
            };
            let initial = if had_previous_surface {
                previous_view
            } else {
                model("")
            };
            let _ = build_launcher_frame(
                &context,
                launcher_input(vec![egui::Event::PointerMoved(before)]),
                &initial,
            );
            let mut pending = painted_web_view(42, 1, &[10; 8]);
            let page = pending.page.as_mut().expect("pending page");
            page.web_surface_id = None;
            page.web_frame = None;
            page.web_generation = 0;
            let _ = build_launcher_frame(
                &context,
                launcher_input(vec![egui::Event::PointerMoved(pending_position)]),
                &pending,
            );
            let admitted = build_launcher_frame(
                &context,
                launcher_input(vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, 3.0),
                    modifiers: egui::Modifiers::NONE,
                }]),
                &painted_web_view(42, 1, &[10; 8]),
            );
            let scrolls: Vec<_> = web_inputs(&admitted)
                .into_iter()
                .filter(|input| matches!(input, WebSurfaceInput::Scroll { .. }))
                .collect();
            assert_eq!(scrolls.len(), usize::from(enters_document),
                "admission must use the pending frame's real final pointer, not its initial seed or predecessor state");
            if enters_document {
                assert!(
                    matches!(scrolls.as_slice(), [WebSurfaceInput::Scroll { surface_id: 42, delta_y: 3.0, x, y, .. }]
                    if (*x - (inside.x - web_document_rect(&admitted).min.x)).abs() < 0.1
                        && (*y - (inside.y - web_document_rect(&admitted).min.y)).abs() < 0.1)
                );
            }
        }
    }
}

#[test]
fn preview_split_and_narrow_detail_keep_results_and_footer_in_bounds() {
    use crikey_ui::PreviewStatus;
    for width in [480.0, 720.0, 1000.0] {
        for height in [260.0, 520.0] {
            for scale in [1.0, 1.5, 2.0] {
                for status in [
                    PreviewStatus::Loading,
                    PreviewStatus::Empty,
                    PreviewStatus::Unavailable("Worker unavailable".into()),
                    PreviewStatus::Error("Worker failed".into()),
                ] {
                    let context = create_launcher_context();
                    context.set_pixels_per_point(scale);
                    let mut view = scrollable_model(20, 0);
                    view.preview.status = Some(status);
                    let input = egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, height),
                        )),
                        focused: true,
                        ..Default::default()
                    };
                    let _ = build_launcher_frame(&context, input.clone(), &view);
                    let frame = build_launcher_frame(&context, input, &view);
                    assert!(painted(&frame, "row-0"));
                    assert!(position_of(&frame, "20 results").y < height - 10.0);
                    if width < 624.0 {
                        assert!(painted(&frame, "Show detail"));
                        assert!(!painted(&frame, "Loading preview"));
                    } else {
                        assert!(painted(&frame, "Detail"));
                        assert!(position_of(&frame, "Detail").x > position_of(&frame, "row-0").x);
                    }
                }
            }
        }
    }
}

#[test]
fn preview_focus_is_explicit_and_back_returns_typing_to_search() {
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    let selection = launcher.preview_selection().unwrap();
    launcher.publish_preview(&selection, crikey_ui::PreviewStatus::Loading);
    let (focused, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![key_press(egui::Key::F6, egui::Modifiers::NONE)],
    );
    assert!(focused.preview.focused);
    assert!(effects.is_empty());
    let (preview_typing, effects) = drive_ordered_frame_input(
        &context,
        &mut launcher,
        focused,
        vec![egui::Event::Text("x".into())],
    );
    assert_eq!(preview_typing.query, "fixture");
    assert!(effects.is_empty());
    let (returned, effects) = drive_ordered_frame_input(
        &context,
        &mut launcher,
        preview_typing,
        vec![
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            key_press(egui::Key::A, egui::Modifiers::COMMAND),
            egui::Event::Text("replacement".into()),
        ],
    );
    assert!(!returned.preview.focused);
    assert_eq!(returned.query, "replacement");
    assert_eq!(effects, vec![crikey_ui::UiEffect::Query("replacement".into())]);
}

#[test]
fn formatted_preview_link_click_reports_host_intent_without_browser_output() {
    use crikey_ui::{prepare_preview, LauncherViewModel, PreviewLinkTarget, PreviewStatus};
    let mut state = LauncherViewModel::new();
    state.activate();
    state.apply(UiCommand::SetQuery("preview".into()));
    state.begin_generation(Generation::from_raw(1));
    state.publish(Generation::from_raw(1), vec![result_row(0)], false);
    let selection = state.preview_selection().unwrap();
    let target = PreviewLinkTarget {
        selection: selection.clone(),
        plugin: selection.plugin.clone(),
        incarnation: 7,
        request: 9,
    };
    let prepared = Arc::new(
        prepare_preview(Arc::new(crikey_core::PreviewContent {
            text: Some(crikey_core::PreviewText::Markdown(
                "# Heading\n\n**Strong** and [Deliberate link](https://example.org/)".into(),
            )),
            ..Default::default()
        }))
        .unwrap(),
    );
    assert!(!prepared.blocks.is_empty());
    state.publish_preview(&selection, PreviewStatus::Content(prepared));
    state.set_preview_link_target(target.clone());
    let view = state.frame().unwrap();
    let context = create_launcher_context();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&frame, "Heading"));
    assert!(painted(&frame, "Strong"));
    let clicked = build_launcher_frame(
        &context,
        launcher_input(click_at(
            position_of(&frame, "Deliberate link") + egui::vec2(5.0, 5.0),
        )),
        &view,
    );
    assert!(clicked.output.platform_output.open_url.is_none());
    assert!(clicked.commands.contains(&UiCommand::OpenPreviewLink {
        target: Box::new(target),
        url: "https://example.org/".into()
    }));
}

#[test]
fn list_and_long_preview_scroll_independently_without_moving_footer() {
    let mut view = scrollable_model(20, 0);
    view.preview.status = Some(crikey_ui::PreviewStatus::Content(Arc::new(
        crikey_ui::prepare_preview(Arc::new(crikey_core::PreviewContent {
            text: Some(crikey_core::PreviewText::Plain(format!(
                "PREVIEW_LONG_BEGIN\n{}",
                "Long bounded detail line\n".repeat(150)
            ))),
            ..Default::default()
        }))
        .unwrap(),
    )));
    let mut frames = Frames::new();
    let initial = frames.settle(&view);
    let list_before = position_of(&initial, "row-0");
    let list_probe_before = position_of(&initial, "row-2");
    let preview_before = position_of(&initial, "PREVIEW_LONG_BEGIN");
    let footer = position_of(&initial, "20 results");
    let _ = frames.draw(
        &view,
        vec![
            egui::Event::PointerMoved(egui::pos2(620.0, 300.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -200.0),
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    let detail_scrolled = frames.settle(&view);
    assert_eq!(position_of(&detail_scrolled, "row-0"), list_before);
    let preview_scrolled = position_of(&detail_scrolled, "PREVIEW_LONG_BEGIN");
    assert!(preview_scrolled.y < preview_before.y);
    // Keep the position probe in view: wheel_over_list deliberately scrolls
    // 400 points and culls row-0 (as the dedicated list-wheel test asserts).
    // A one-row gesture still proves motion without querying off-screen text.
    let _ = frames.draw(
        &view,
        vec![
            egui::Event::PointerMoved(list_probe_before + egui::vec2(5.0, 5.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -40.0),
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    let list_scrolled = frames.settle(&view);
    assert!(position_of(&list_scrolled, "row-2").y < list_probe_before.y);
    assert_eq!(
        position_of(&list_scrolled, "PREVIEW_LONG_BEGIN"),
        preview_scrolled
    );
    assert_eq!(position_of(&list_scrolled, "20 results"), footer);
}

#[test]
fn narrow_detail_offers_back_instead_of_compressing_the_result_list() {
    let context = create_launcher_context();
    let mut view = scrollable_model(20, 0);
    view.preview.status = Some(crikey_ui::PreviewStatus::Loading);
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(480.0, 520.0),
        )),
        focused: true,
        ..Default::default()
    };
    let _ = build_launcher_frame(&context, input(), &view);
    let list = build_launcher_frame(&context, input(), &view);
    assert!(painted(&list, "Show detail"));
    assert!(painted(&list, "row-0"));
    view.preview.detail = true;
    view.preview.focused = true;
    let _ = build_launcher_frame(&context, input(), &view);
    let detail = build_launcher_frame(&context, input(), &view);
    assert!(painted(&detail, "Back to results"));
    assert!(painted(&detail, "Loading preview"));
    assert!(!painted(&detail, "row-0"));
    assert!(position_of(&detail, "20 results").y < 510.0);
    let mut back_input = input();
    back_input.events = click_at(position_of(&detail, "Back to results") + egui::vec2(5.0, 5.0));
    let back = build_launcher_frame(&context, back_input, &view);
    assert!(back.commands.contains(&UiCommand::LeavePreview));
    assert!(!back
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetQuery(_))));
}

#[test]
fn preview_divider_drag_reports_one_bounded_remembered_ratio() {
    let mut view = scrollable_model(20, 0);
    view.preview.status = Some(crikey_ui::PreviewStatus::Loading);
    let mut frames = Frames::new();
    let initial = frames.settle(&view);
    fn divider_position(shape: &egui::Shape) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::LineSegment { points, .. }
                if (points[0].x - points[1].x).abs() < 0.1 && (points[0].y - points[1].y).abs() > 40.0 =>
            {
                Some(egui::pos2(points[0].x, (points[0].y + points[1].y) / 2.0))
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(divider_position),
            _ => None,
        }
    }
    let divider = initial
        .output
        .shapes
        .iter()
        .find_map(|shape| divider_position(&shape.shape))
        .expect("the real preview divider is painted");
    let pressed = frames.draw(
        &view,
        vec![
            egui::Event::PointerMoved(divider),
            egui::Event::PointerButton {
                pos: divider,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    assert!(!pressed
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetPreviewRatio(_))));
    let moved = egui::pos2(divider.x + 80.0, divider.y);
    let dragging = frames.draw(&view, vec![egui::Event::PointerMoved(moved)]);
    assert!(!dragging
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetPreviewRatio(_))));
    let released = frames.draw(
        &view,
        vec![egui::Event::PointerButton {
            pos: moved,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    let ratios: Vec<f32> = released
        .commands
        .iter()
        .filter_map(|command| {
            if let UiCommand::SetPreviewRatio(ratio) = command {
                Some(*ratio)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ratios.len(), 1);
    assert!((0.3..=0.7).contains(&ratios[0]));
    assert!(ratios[0] > view.preview.ratio);
}

fn image_preview(view: &mut ViewModel, color: [u8; 4]) -> std::sync::Weak<crikey_core::PreviewContent> {
    let content = Arc::new(crikey_core::PreviewContent {
        image: Some(crikey_core::PreviewImage {
            width: RASTER[0] as u32,
            height: RASTER[1] as u32,
            rgba: color.repeat(RASTER[0] * RASTER[1]),
        }),
        ..Default::default()
    });
    let weak = Arc::downgrade(&content);
    view.preview.status = Some(crikey_ui::PreviewStatus::Content(Arc::new(
        crikey_ui::prepare_preview(content).unwrap(),
    )));
    weak
}

fn preview_frame(context: &egui::Context, view: &ViewModel, width: f32) -> crikey_ui::NativeUiFrame {
    build_launcher_frame(
        context,
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 520.0),
            )),
            focused: true,
            ..Default::default()
        },
        view,
    )
}

#[test]
fn invisible_preview_retires_its_pixels_and_the_exact_texture_it_painted() {
    for transition in [
        "loading",
        "text",
        "none",
        "empty",
        "error",
        "unavailable",
        "settings",
        "page",
        "narrow-results",
        "no-results",
        "hide",
    ] {
        let context = create_launcher_context();
        let mut view = scrollable_model(2, 0);
        let pixels = image_preview(&mut view, [31, 170, 89, 255]);
        let opened = preview_frame(&context, &view, 720.0);
        let id = raster_texture(&opened);
        let mut sampled = Vec::new();
        for shape in &opened.output.shapes {
            textured_rects(&shape.shape, &mut sampled);
        }
        assert!(
            sampled.iter().any(|(texture, _)| *texture == id),
            "{transition}: image must actually be painted"
        );
        let mut width = 720.0;
        match transition {
            "loading" => {
                view.selected = 1;
                view.preview.status = Some(crikey_ui::PreviewStatus::Loading);
            }
            "text" => {
                view.preview.status = Some(crikey_ui::PreviewStatus::Content(Arc::new(
                    crikey_ui::prepare_preview(Arc::new(crikey_core::PreviewContent {
                        text: Some(crikey_core::PreviewText::Plain("Replacement detail".into())),
                        ..Default::default()
                    }))
                    .unwrap(),
                )))
            }
            "none" => view.preview.status = None,
            "empty" => view.preview.status = Some(crikey_ui::PreviewStatus::Empty),
            "error" => view.preview.status = Some(crikey_ui::PreviewStatus::Error("failed".into())),
            "unavailable" => {
                view.preview.status = Some(crikey_ui::PreviewStatus::Unavailable("unsupported".into()))
            }
            "settings" => view.settings_open = true,
            "page" => view.page = page_view(vec![probe_node()]).page,
            "narrow-results" => width = 480.0,
            "no-results" => view.rows = Arc::default(),
            "hide" => {
                let retired = crikey_ui::release_launcher_textures(&context);
                assert!(
                    retired.contains(&id),
                    "hide must immediately tell the GPU consumer to free the painted texture"
                );
            }
            _ => unreachable!(),
        }
        let hidden = model("");
        let retired = preview_frame(
            &context,
            if transition == "hide" { &hidden } else { &view },
            width,
        );
        assert!(
            retired.output.textures_delta.free.contains(&id),
            "{transition}: exact painted texture must be freed"
        );
        let mut sampled = Vec::new();
        for shape in &retired.output.shapes {
            textured_rects(&shape.shape, &mut sampled);
        }
        assert!(
            !sampled.iter().any(|(texture, _)| *texture == id),
            "{transition}: retired image cannot remain painted"
        );
        if transition == "text" {
            assert!(painted(&retired, "Replacement detail"));
        }
        // Covered models may legitimately own their content, but the renderer
        // must not keep the producer's pixels alive after that model is gone.
        drop(view);
        assert!(
            pixels.upgrade().is_none(),
            "{transition}: renderer retained retired CPU image"
        );
    }
}

#[test]
fn split_and_narrow_detail_share_one_current_image_and_retire_it_on_replacement() {
    let context = create_launcher_context();
    let mut view = scrollable_model(2, 0);
    view.preview.detail = true;
    let previous = image_preview(&mut view, [41, 150, 99, 255]);
    let opened = preview_frame(&context, &view, 720.0);
    let old_id = raster_texture(&opened);
    for width in [480.0, 720.0, 480.0, 1000.0] {
        let frame = preview_frame(&context, &view, width);
        let mut sampled = Vec::new();
        for shape in &frame.output.shapes {
            textured_rects(&shape.shape, &mut sampled);
        }
        assert!(
            sampled.iter().any(|(id, _)| *id == old_id),
            "visible detail must reuse the same texture across layouts"
        );
        assert!(!frame
            .output
            .textures_delta
            .set
            .iter()
            .any(|(_, delta)| delta.image.size() == RASTER));
        assert!(!frame.output.textures_delta.free.contains(&old_id));
    }
    let current = image_preview(&mut view, [210, 42, 61, 255]);
    assert!(
        previous.upgrade().is_none(),
        "no layout cache may retain old producer pixels"
    );
    let replacement = preview_frame(&context, &view, 720.0);
    let new_id = raster_texture(&replacement);
    assert_ne!(new_id, old_id);
    assert!(replacement.output.textures_delta.free.contains(&old_id));
    view.preview.status = None;
    let closed = preview_frame(&context, &view, 480.0);
    assert!(closed.output.textures_delta.free.contains(&new_id));
    assert!(current.upgrade().is_none());
}

fn action_launcher() -> crikey_ui::LauncherViewModel {
    use crikey_core::{Action, ActionId, ExecutionPolicy};
    let mut row = result_row(0);
    let action = |id: &str, label: &str| Action {
        action_id: ActionId(id.into()),
        label: label.into(),
        description: String::new(),
        applicable_categories: Vec::new(),
        icon_reference: None,
        execution_policy: ExecutionPolicy::Plugin,
    };
    row.default_action = Some(action("run", "Run selected"));
    row.alternate_actions = vec![
        action("copy", "Copy selected"),
        action("reveal", "Reveal selected"),
    ];
    let mut launcher = crikey_ui::LauncherViewModel::new();
    launcher.activate();
    let _ = launcher.apply(UiCommand::SetQuery("fixture".into()));
    launcher.begin_generation(Generation::from_raw(1));
    launcher.publish(Generation::from_raw(1), vec![row], false);
    launcher
}

#[test]
fn rendered_action_search_executes_non_first_leaf_and_does_not_edit_launcher_query() {
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    let root = launcher.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(vec![]), &root);
    let discovery = build_launcher_frame(
        &context,
        launcher_input(vec![key_press(egui::Key::K, egui::Modifiers::COMMAND)]),
        &root,
    );
    let mut generation = 1;
    let mut effects = Vec::new();
    for command in discovery.commands {
        apply_ordered_test_command(&mut launcher, command, &root.rows, &mut generation, &mut effects);
    }
    let open = launcher.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(vec![]), &open);
    let typed = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::Text("Reveal".into())]),
        &open,
    );
    for command in typed.commands {
        apply_ordered_test_command(&mut launcher, command, &root.rows, &mut generation, &mut effects);
    }
    let filtered = launcher.frame().unwrap();
    assert_eq!(filtered.query, "fixture");
    assert_eq!(filtered.action_panel.as_ref().unwrap().query(), "Reveal");
    let painted = build_launcher_frame(&context, launcher_input(vec![]), &filtered);
    assert!(painted
        .output
        .shapes
        .iter()
        .any(|shape| contains_text(&shape.shape, "Reveal selected")));
    assert!(!painted
        .output
        .shapes
        .iter()
        .any(|shape| contains_text(&shape.shape, "Copy selected")));
    let execute = build_launcher_frame(
        &context,
        launcher_input(vec![key_press(egui::Key::Enter, egui::Modifiers::NONE)]),
        &filtered,
    );
    for command in execute.commands {
        apply_ordered_test_command(&mut launcher, command, &root.rows, &mut generation, &mut effects);
    }
    assert_eq!(
        effects,
        vec![crikey_ui::UiEffect::Execute {
            owner: PluginId("renderer-fixture".into()),
            item: ItemId("item-0".into()),
            action: crikey_core::ActionId("reveal".into()),
        }]
    );
}

fn long_action_launcher() -> crikey_ui::LauncherViewModel {
    use crikey_core::{Action, ActionId, ExecutionPolicy};
    let mut row = result_row(0);
    row.alternate_actions = (0..40)
        .map(|index| Action {
            action_id: ActionId(format!("long-{index}")),
            label: format!("Long action {index:02}"),
            description: String::new(),
            applicable_categories: Vec::new(),
            icon_reference: None,
            execution_policy: ExecutionPolicy::Plugin,
        })
        .collect();
    let mut launcher = crikey_ui::LauncherViewModel::new();
    launcher.activate();
    let _ = launcher.apply(UiCommand::SetQuery("fixture".into()));
    launcher.begin_generation(Generation::from_raw(1));
    launcher.publish(Generation::from_raw(1), vec![row], false);
    let _ = launcher.apply(UiCommand::ShowActions);
    launcher
}

fn wheel_over_action(frame: &crikey_ui::NativeUiFrame, label: &str) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(position_of(frame, label) + egui::vec2(2.0, 4.0)),
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -400.0),
            modifiers: egui::Modifiers::NONE,
        },
    ]
}

#[test]
fn action_sheet_wheel_stays_scrolled_until_keyboard_selection_changes() {
    let mut launcher = long_action_launcher();
    let view = launcher.frame().unwrap();
    let mut frames = Frames::new();
    let first = frames.settle(&view);
    assert!(painted(&first, "Long action 00"));
    let _ = frames.draw(&view, wheel_over_action(&first, "Long action 00"));
    let scrolled = frames.settle(&view);
    assert!(
        !painted(&scrolled, "Long action 00"),
        "stationary frames must not undo scrolling away from the selection"
    );
    let keyboard = frames.draw(
        &view,
        vec![key_press(egui::Key::ArrowDown, egui::Modifiers::NONE)],
    );
    let mut generation = 1;
    let mut effects = Vec::new();
    for command in keyboard.commands {
        apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
    }
    assert!(effects.is_empty());
    let moved = launcher.frame().unwrap();
    assert_eq!(
        moved.action_panel.as_ref().unwrap().selected(),
        Some(&crikey_ui::action_panel::ActionIdentity::Plugin(
            crikey_core::ActionId("long-1".into())
        ))
    );
    assert!(painted(&frames.settle(&moved), "Long action 01"));
}

#[test]
fn action_sheet_search_clamps_a_scrolled_viewport_and_restores_browse_selection() {
    let mut launcher = long_action_launcher();
    let view = launcher.frame().unwrap();
    let mut frames = Frames::new();
    let first = frames.settle(&view);
    let _ = frames.draw(&view, wheel_over_action(&first, "Long action 00"));
    assert!(!painted(&frames.settle(&view), "Long action 00"));
    let typed = frames.draw(&view, vec![egui::Event::Text("Long action 39".into())]);
    let mut generation = 1;
    let mut effects = Vec::new();
    for command in typed.commands {
        apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
    }
    let filtered = launcher.frame().unwrap();
    assert_eq!(filtered.action_panel.as_ref().unwrap().visible_entries().len(), 1);
    assert!(painted(&frames.settle(&filtered), "Long action 39"));
    let escape = frames.draw(
        &filtered,
        vec![key_press(egui::Key::Escape, egui::Modifiers::NONE)],
    );
    for command in escape.commands {
        apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
    }
    assert!(effects.is_empty());
    let restored = launcher.frame().unwrap();
    assert_eq!(
        restored.action_panel.as_ref().unwrap().selected(),
        view.action_panel.as_ref().unwrap().selected()
    );
    assert!(painted(&frames.settle(&restored), "Long action 00"));
}

fn action_text_is_visible(frame: &crikey_ui::NativeUiFrame, label: &str) -> bool {
    fn text_rect(shape: &egui::Shape, label: &str) -> Option<egui::Rect> {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| text_rect(shape, label)),
            _ => None,
        }
    }
    frame.output.shapes.iter().any(|clipped| {
        text_rect(&clipped.shape, label).is_some_and(|rect| clipped.clip_rect.contains_rect(rect))
    })
}

#[test]
fn action_sheet_multihit_search_reveals_a_retained_selection_inside_its_clip() {
    let mut launcher = long_action_launcher();
    let mut frames = Frames::new();
    let _ = frames.settle(&launcher.frame().unwrap());
    for _ in 0..20 {
        let _ = launcher.apply(UiCommand::SelectNext);
    }
    let view = launcher.frame().unwrap();
    let selected = view.action_panel.as_ref().unwrap().selected().cloned();
    assert!(action_text_is_visible(&frames.settle(&view), "Long action 20"));
    let typed = frames.draw(&view, vec![egui::Event::Text("Long action 2".into())]);
    for command in typed.commands {
        let _ = launcher.apply(command);
    }
    let filtered = launcher.frame().unwrap();
    let panel = filtered.action_panel.as_ref().unwrap();
    assert_eq!(panel.selected(), selected.as_ref());
    assert!(panel.visible_entries().len() > 1);
    let revealed = frames.settle(&filtered);
    assert!(
        action_text_is_visible(&revealed, "Long action 20"),
        "filtering must reveal a retained identity, not leave its galley clipped away"
    );
    let _ = frames.draw(&filtered, wheel_over_action(&revealed, "Long action 20"));
    assert!(
        !action_text_is_visible(&frames.settle(&filtered), "Long action 20"),
        "a completed layout transition must not keep overriding later wheel scrolling"
    );
}

#[test]
fn action_sheet_opening_does_not_synthesize_selection_navigation() {
    use crikey_ui::action_panel::ActionIdentity;
    let mut launcher = long_action_launcher();
    let mut view = launcher.frame().unwrap();
    let selected = ActionIdentity::Plugin(crikey_core::ActionId("long-39".into()));
    assert!(Arc::make_mut(view.action_panel.as_mut().unwrap()).select(&selected));
    let mut frames = Frames::new();
    let opened = frames.settle(&view);
    assert!(
        action_text_is_visible(&opened, "Long action 00"),
        "opening establishes a selection baseline without scheduling an animated scroll"
    );
    assert!(!action_text_is_visible(&opened, "Long action 39"));
}

#[test]
fn action_sheet_reopening_starts_at_the_top_without_following_the_old_selection() {
    let mut launcher = long_action_launcher();
    let view = launcher.frame().unwrap();
    let mut frames = Frames::new();
    let opened = frames.settle(&view);
    let _ = frames.draw(&view, wheel_over_action(&opened, "Long action 00"));
    assert!(!action_text_is_visible(&frames.settle(&view), "Long action 00"));
    let _ = launcher.apply(UiCommand::ActionBack);
    let closed = launcher.frame().unwrap();
    assert!(closed.action_panel.is_none());
    let _ = frames.draw(&closed, Vec::new());
    let _ = launcher.apply(UiCommand::ShowActions);
    let reopened = launcher.frame().unwrap();
    assert_eq!(
        reopened.action_panel.as_ref().unwrap().selected(),
        view.action_panel.as_ref().unwrap().selected()
    );
    let first = frames.draw(&reopened, Vec::new());
    assert!(
        action_text_is_visible(&first, "Long action 00"),
        "a fresh sheet must start at the top, not inherit the retired sheet's viewport"
    );
    assert!(action_text_is_visible(
        &frames.settle(&reopened),
        "Long action 00"
    ));
    let _ = frames.draw(&reopened, wheel_over_action(&first, "Long action 00"));
    assert!(
        !action_text_is_visible(&frames.settle(&reopened), "Long action 00"),
        "the reset must happen only on opening, not on subsequent stationary frames"
    );
}

#[test]
fn action_sheet_unchanged_provider_refresh_preserves_manual_scroll() {
    let mut launcher = long_action_launcher();
    let view = launcher.frame().unwrap();
    let mut frames = Frames::new();
    let opened = frames.settle(&view);
    let _ = frames.draw(&view, wheel_over_action(&opened, "Long action 00"));
    let scrolled = frames.settle(&view);
    assert!(!action_text_is_visible(&scrolled, "Long action 00"));
    let anchor = (1..40)
        .map(|index| format!("Long action {index:02}"))
        .find(|label| action_text_is_visible(&scrolled, label))
        .expect("manual scrolling leaves a visible action to anchor");
    let before = position_of(&scrolled, &anchor);
    launcher.publish(Generation::from_raw(1), view.rows.to_vec(), false);
    let refreshed = launcher.frame().unwrap();
    assert_eq!(
        refreshed.action_panel.as_ref().unwrap().selected(),
        view.action_panel.as_ref().unwrap().selected()
    );
    let after = frames.settle(&refreshed);
    assert!(!action_text_is_visible(&after, "Long action 00"));
    assert!(action_text_is_visible(&after, &anchor));
    assert!((position_of(&after, &anchor).y - before.y).abs() < 1.0);
}

#[test]
fn action_sheet_changed_provider_layout_reveals_the_retained_identity() {
    let mut launcher = long_action_launcher();
    let mut frames = Frames::new();
    let _ = frames.settle(&launcher.frame().unwrap());
    for _ in 0..20 {
        let _ = launcher.apply(UiCommand::SelectNext);
    }
    let view = launcher.frame().unwrap();
    assert!(action_text_is_visible(&frames.settle(&view), "Long action 20"));
    let mut rows = view.rows.to_vec();
    rows[0].alternate_actions.rotate_left(20);
    launcher.publish(Generation::from_raw(1), rows, false);
    let refreshed = launcher.frame().unwrap();
    assert_eq!(
        refreshed.action_panel.as_ref().unwrap().selected(),
        view.action_panel.as_ref().unwrap().selected()
    );
    assert!(action_text_is_visible(
        &frames.settle(&refreshed),
        "Long action 20"
    ));
}

#[test]
fn action_sheet_submenu_back_reveals_the_exact_parent_selection() {
    use crikey_ui::action_panel::{
        ActionActivation, ActionEntry, ActionIdentity, ActionPanelNavigation, ActionPresentation,
        ActionSection, ActionTree,
    };
    let mut launcher = long_action_launcher();
    let mut view = launcher.frame().unwrap();
    let leaf = |index: usize| ActionEntry::Plugin {
        action: crikey_core::ActionId(format!("long-{index}")),
        presentation: ActionPresentation {
            label: format!("Long action {index:02}"),
            ..Default::default()
        },
    };
    let mut entries: Vec<_> = (0..39).map(leaf).collect();
    entries.push(ActionEntry::Submenu {
        id: "last-menu".into(),
        presentation: ActionPresentation {
            label: "Last submenu".into(),
            ..Default::default()
        },
        entries: vec![leaf(39)],
    });
    let tree = ActionTree::new(
        &view.rows[0],
        vec![ActionSection {
            id: "long".into(),
            label: "Actions".into(),
            entries,
        }],
    )
    .unwrap();
    view.action_panel = Some(Arc::new(crikey_ui::ActionPanel::new(tree)));
    let mut frames = Frames::new();
    let _ = frames.settle(&view);
    let parent = ActionIdentity::Submenu("last-menu".into());
    assert!(Arc::make_mut(view.action_panel.as_mut().unwrap()).select(&parent));
    assert!(painted(&frames.settle(&view), "Last submenu"));
    assert_eq!(
        Arc::make_mut(view.action_panel.as_mut().unwrap()).activation(&view.rows[0]),
        Ok(Some(ActionActivation::EnteredSubmenu))
    );
    assert!(painted(&frames.settle(&view), "Long action 39"));
    assert_eq!(
        Arc::make_mut(view.action_panel.as_mut().unwrap()).back(),
        ActionPanelNavigation::Changed
    );
    assert_eq!(view.action_panel.as_ref().unwrap().selected(), Some(&parent));
    assert!(painted(&frames.settle(&view), "Last submenu"));
}

#[test]
fn discovery_shortcuts_do_not_steal_settings_page_or_composition_input() {
    for (key, modifiers, expected_keysym, expected_modifiers) in [
        (
            egui::Key::K,
            egui::Modifiers::CTRL | egui::Modifiers::COMMAND,
            u32::from(b'k'),
            crikey_ui::WEB_MODIFIER_CONTROL,
        ),
        (
            egui::Key::K,
            egui::Modifiers::MAC_CMD | egui::Modifiers::COMMAND,
            u32::from(b'k'),
            crikey_ui::WEB_MODIFIER_META,
        ),
        (
            egui::Key::Enter,
            egui::Modifiers::ALT,
            0xff0d,
            crikey_ui::WEB_MODIFIER_ALT,
        ),
    ] {
        for surface in 0..3 {
            let context = create_launcher_context();
            let mut view = match surface {
                0 => hotkey_settings_model(),
                1 => page_view(vec![]),
                _ => painted_web_view(41, 1, &[0; 8]),
            };
            view.query = "retained".into();
            let _ = build_launcher_frame(&context, launcher_input(vec![]), &view);
            let frame =
                build_launcher_frame(&context, launcher_input(vec![key_press(key, modifiers)]), &view);
            assert!(!frame.commands.contains(&UiCommand::ShowActions));
            if surface == 2 {
                assert!(web_inputs(&frame).iter().any(|input| matches!(
                    input, WebSurfaceInput::Key { surface_id: 41, pressed: true, keysym, modifiers, .. }
                        if *keysym == expected_keysym && *modifiers == expected_modifiers
                )));
            }
        }
        let context = create_launcher_context();
        let mut launcher = action_launcher();
        let root = launcher.frame().unwrap();
        let _ = build_launcher_frame(&context, launcher_input(vec![]), &root);
        let frame = build_launcher_frame(
            &context,
            launcher_input(vec![
                egui::Event::Ime(egui::ImeEvent::Enabled),
                egui::Event::Ime(egui::ImeEvent::Preedit("composing".into())),
                key_press(key, modifiers),
            ]),
            &root,
        );
        assert!(!frame.commands.contains(&UiCommand::ShowActions));
    }
}

#[test]
fn explicit_destructive_presentation_and_shortcuts_are_visible_without_dispatch() {
    use crikey_ui::action_panel::{ActionEntry, ActionPresentation, ActionSection, ActionTree};
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    let mut view = launcher.frame().unwrap();
    let tree = ActionTree::new(
        &view.rows[0],
        vec![ActionSection {
            id: "explicit".into(),
            label: "Explicit section".into(),
            entries: vec![ActionEntry::Plugin {
                action: crikey_core::ActionId("copy".into()),
                presentation: ActionPresentation {
                    label: "Remove saved entry".into(),
                    destructive: true,
                    shortcut: Some("Ctrl+Shift+R".into()),
                    icon_reference: Some("unresolved-resource".into()),
                    ..Default::default()
                },
            }],
        }],
    )
    .unwrap();
    view.action_panel = Some(Arc::new(crikey_ui::ActionPanel::new(tree)));
    let frame = build_launcher_frame(&context, launcher_input(vec![]), &view);
    for text in [
        "Explicit section",
        "Remove saved entry",
        "Destructive",
        "Ctrl+Shift+R",
    ] {
        assert!(
            frame
                .output
                .shapes
                .iter()
                .any(|shape| contains_text(&shape.shape, text)),
            "{text}"
        );
    }
    assert!(
        frame
            .commands
            .iter()
            .all(|command| matches!(command, UiCommand::SetLogicalScroll(_))),
        "presentation is not authorization or activation"
    );
}

#[test]
fn batched_action_text_and_return_preserve_their_actual_event_order() {
    for text_before_return in [true, false] {
        let context = create_launcher_context();
        let mut launcher = action_launcher();
        let _ = launcher.apply(UiCommand::ShowActions);
        let view = launcher.frame().unwrap();
        let _ = build_launcher_frame(&context, launcher_input(vec![]), &view);
        let text = egui::Event::Text("Reveal".into());
        let enter = key_press(egui::Key::Enter, egui::Modifiers::NONE);
        let events = if text_before_return {
            vec![text, enter]
        } else {
            vec![enter, text]
        };
        let frame = build_launcher_frame(&context, launcher_input(events), &view);
        let mut effects = Vec::new();
        let mut generation = 1;
        for command in frame.commands {
            apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
        }
        assert_eq!(
            effects,
            vec![crikey_ui::UiEffect::Execute {
                owner: PluginId("renderer-fixture".into()),
                item: ItemId("item-0".into()),
                action: crikey_core::ActionId(if text_before_return { "reveal" } else { "run" }.into()),
            }]
        );
        assert_eq!(launcher.frame().unwrap().query, "fixture");
    }
}

#[test]
fn composition_owned_return_followed_by_commit_never_executes_an_action() {
    for composition_started_in_previous_frame in [false, true] {
        let context = create_launcher_context();
        let mut launcher = action_launcher();
        let _ = launcher.apply(UiCommand::ShowActions);
        let mut view = launcher.frame().unwrap();
        let _ = build_launcher_frame(&context, launcher_input(vec![]), &view);
        let preedit = vec![
            egui::Event::Ime(egui::ImeEvent::Enabled),
            egui::Event::Ime(egui::ImeEvent::Preedit("Re".into())),
        ];
        let mut events = if composition_started_in_previous_frame {
            let frame = build_launcher_frame(&context, launcher_input(preedit), &view);
            for command in frame.commands {
                assert!(launcher.apply(command).is_none());
            }
            view = launcher.frame().unwrap();
            Vec::new()
        } else {
            preedit
        };
        events.push(key_press(egui::Key::Enter, egui::Modifiers::NONE));
        events.push(egui::Event::Ime(egui::ImeEvent::Commit("Reveal".into())));
        let frame = build_launcher_frame(&context, launcher_input(events), &view);
        assert!(!frame.commands.contains(&UiCommand::ActivateSelectedAction));
        assert!(!frame
            .commands
            .iter()
            .any(|command| matches!(command, UiCommand::SetQuery(_))));
        for command in frame.commands {
            assert!(launcher.apply(command).is_none());
        }
        let frame = launcher.frame().unwrap();
        assert_eq!(frame.query, "fixture");
        assert_eq!(frame.action_panel.as_ref().unwrap().query(), "Reveal");
    }
}

#[test]
fn action_failure_replaces_long_metadata_and_exposes_its_complete_reason_on_hover() {
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    let mut view = launcher.frame().unwrap();
    let mut rows = view.rows.to_vec();
    rows[0].description =
        "A long provider description that would otherwise consume the entire line ".repeat(20);
    let diagnostic = format!(
        "Action failed: {} terminal diagnostic detail",
        "specific cause ".repeat(30)
    );
    rows[0].status = Some(diagnostic.clone());
    view.rows = rows.into();
    let input = |time, events| {
        let mut input = launcher_input(events);
        input.time = Some(time);
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(480.0, 520.0),
        ));
        input
    };
    let first = build_launcher_frame(&context, input(0.0, vec![]), &view);
    let status = position_of(&first, "Action failed");
    assert!(
        status.x < 200.0,
        "failure feedback must start before lengthy row metadata"
    );
    assert!(!painted(&first, "A long provider description"));
    assert!(position_of(&first, "Settings").y < 520.0);
    let pointer = status + egui::vec2(4.0, 4.0);
    let _ = build_launcher_frame(
        &context,
        input(1.0, vec![egui::Event::PointerMoved(pointer)]),
        &view,
    );
    let hovered = build_launcher_frame(&context, input(3.0, vec![]), &view);
    let count = |frame: &crikey_ui::NativeUiFrame| {
        frame
            .output
            .shapes
            .iter()
            .filter(|shape| contains_text(&shape.shape, &diagnostic))
            .count()
    };
    assert!(
        count(&hovered) > count(&first),
        "hover must expose the full diagnostic in a real tooltip"
    );
    assert!(position_of(&hovered, "Settings").y < 520.0);
}

#[test]
fn navigation_settings_back_and_history_clear_are_visible_host_controls() {
    let mut state = crikey_ui::LauncherViewModel::new();
    state.activate();
    state.open_settings(None);
    let view = state.frame().unwrap();
    let mut frames = Frames::new();
    let drawn = frames.settle(&view);
    assert!(painted(&drawn, "Settings"));
    let clear = position_of(&drawn, "Clear query history") + egui::vec2(4.0, 4.0);
    let cleared = frames.draw(&view, click_at(clear));
    assert!(cleared.commands.contains(&UiCommand::ClearQueryHistory));
    let back = position_of(&drawn, "Back") + egui::vec2(4.0, 4.0);
    let returned = frames.draw(&view, click_at(back));
    assert!(returned.commands.contains(&UiCommand::CloseSettings));
}

#[test]
fn recall_picker_click_restores_text_without_an_execution_intent() {
    let mut state = crikey_ui::LauncherViewModel::new();
    state.activate();
    state.set_query_recall(vec!["recent submission".into(), "older submission".into()]);
    let view = state.frame().unwrap();
    let mut frames = Frames::new();
    let drawn = frames.settle(&view);
    assert!(painted(&drawn, "Submitted searches"));
    let older = position_of(&drawn, "older submission") + egui::vec2(4.0, 4.0);
    let selected = frames.draw(&view, click_at(older));
    assert!(selected
        .commands
        .contains(&UiCommand::SetQuery("older submission".into())));
    assert!(!selected.commands.contains(&UiCommand::ExecuteDefault));
}

#[test]
fn empty_root_recall_does_not_steal_an_ime_candidate_arrow() {
    let context = create_launcher_context();
    let view = model("");
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            egui::Event::Ime(egui::ImeEvent::Enabled),
            key_press(egui::Key::ArrowUp, egui::Modifiers::NONE),
            egui::Event::Ime(egui::ImeEvent::Preedit("に".into())),
        ]),
        &view,
    );
    assert!(!frame.commands.contains(&UiCommand::RecallQuery));
}

#[test]
fn recall_text_and_return_preserve_both_actual_event_orders() {
    for text_before_return in [true, false] {
        let context = create_launcher_context();
        let mut state = crikey_ui::LauncherViewModel::new();
        state.activate();
        state.set_query_recall(vec!["previous submission".into()]);
        let view = state.frame().unwrap();
        let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let text = egui::Event::Text("fresh query".into());
        let enter = key_press(egui::Key::Enter, egui::Modifiers::NONE);
        let events = if text_before_return {
            vec![text, enter]
        } else {
            vec![enter, text]
        };
        let frame = build_launcher_frame(&context, launcher_input(events), &view);
        let effects: Vec<_> = frame
            .commands
            .into_iter()
            .filter_map(|command| state.apply(command))
            .collect();
        let expected = if text_before_return {
            "fresh query"
        } else {
            "previous submission"
        };
        assert_eq!(effects, vec![crikey_ui::UiEffect::Query(expected.into())]);
        assert_eq!(state.frame().unwrap().query, expected);
    }
}

fn ordered_action_launcher() -> crikey_ui::LauncherViewModel {
    let mut launcher = action_launcher();
    let root = launcher.frame().unwrap();
    let mut row = root.rows[0].clone();
    row.default_action.as_mut().unwrap().label = "Run first".into();
    row.alternate_actions[0].label = "Copy target".into();
    row.alternate_actions[1].label = "Copy alternate".into();
    launcher.publish(root.generation, vec![row], false);
    launcher.apply(UiCommand::ShowActions);
    launcher
}

fn apply_ordered_test_command(
    launcher: &mut crikey_ui::LauncherViewModel,
    command: UiCommand,
    rows: &[ResultRow],
    generation: &mut u64,
    effects: &mut Vec<crikey_ui::UiEffect>,
) {
    let mut effect = launcher.apply(command);
    loop {
        match effect {
            Some(crikey_ui::UiEffect::InputBatch {
                continuation,
                commands,
            }) => {
                for command in commands {
                    apply_ordered_test_command(launcher, command, rows, generation, effects);
                }
                if let Some(token) = continuation {
                    launcher.finish_input_batch(token);
                }
            }
            Some(effect) => {
                if matches!(&effect, crikey_ui::UiEffect::Query(_)) {
                    // The existing synchronous root consumer admits/publishes
                    // its answer before the following command, as the host does.
                    *generation += 1;
                    let current = Generation::from_raw(*generation);
                    launcher.begin_generation(current);
                    launcher.publish(current, rows.to_vec(), false);
                }
                effects.push(effect);
            }
            None => {}
        }
        if let Some(raw) = launcher.take_query_refresh() {
            effect = Some(crikey_ui::UiEffect::Query(raw));
        } else {
            break;
        }
    }
}

fn drive_ordered_action_input(
    context: &egui::Context,
    launcher: &mut crikey_ui::LauncherViewModel,
    events: Vec<egui::Event>,
) -> (ViewModel, Vec<crikey_ui::UiEffect>) {
    let view = launcher.frame().unwrap();
    drive_ordered_frame_input(context, launcher, view, events)
}

fn drive_ordered_frame_input(
    context: &egui::Context,
    launcher: &mut crikey_ui::LauncherViewModel,
    mut view: ViewModel,
    events: Vec<egui::Event>,
) -> (ViewModel, Vec<crikey_ui::UiEffect>) {
    // Each host key consumes one boundary; a fresh Settings route needs one
    // ordinary focus-establishing paint. No sleeps or timing assumptions.
    let turns = events.len() + 2;
    let rows = view.rows.clone();
    let _ = build_launcher_frame(context, launcher_input(vec![]), &view);
    let mut input = launcher_input(events);
    let mut effects = Vec::new();
    let mut generation = 1;
    for _ in 0..turns {
        let frame = build_launcher_frame(context, input, &view);
        input = launcher_input(vec![]);
        for command in frame.commands {
            apply_ordered_test_command(launcher, command, &rows, &mut generation, &mut effects);
        }
        view = launcher.frame().unwrap_or(view);
    }
    (view, effects)
}

fn ordered_execution(action: &str) -> crikey_ui::UiEffect {
    crikey_ui::UiEffect::Execute {
        owner: PluginId("renderer-fixture".into()),
        item: ItemId("item-0".into()),
        action: crikey_core::ActionId(action.into()),
    }
}

#[test]
fn ordered_remapped_navigation_preserves_text_then_navigation_then_activation() {
    use crikey_ui::navigation::{CompiledNavigation, NavigationStyle};
    for actions in [false, true] {
        let context = create_launcher_context();
        crikey_ui::set_launcher_navigation(
            &context,
            Arc::new(CompiledNavigation::compile(NavigationStyle::Emacs, "{}").unwrap()),
        );
        let mut launcher = ordered_action_launcher();
        if !actions {
            launcher.apply(UiCommand::ActionBack);
            let view = launcher.frame().unwrap();
            let first = view.rows[0].clone();
            let mut second = first.clone();
            second.target = crikey_ui::ActionTarget::Result {
                owner: PluginId("renderer-fixture".into()),
                item: ItemId("item-1".into()),
            };
            second.source.as_mut().unwrap().item = ItemId("item-1".into());
            launcher.publish(view.generation, vec![first, second], false);
        }
        let (view, effects) = drive_ordered_action_input(
            &context,
            &mut launcher,
            vec![
                egui::Event::Text("Copy".into()),
                key_press(egui::Key::N, egui::Modifiers::CTRL),
                key_press(egui::Key::Enter, egui::Modifiers::NONE),
            ],
        );
        if actions {
            assert_eq!(effects, vec![ordered_execution("reveal")]);
            assert_eq!(view.query, "fixture");
        } else {
            assert_eq!(view.query, "fixtureCopy");
            assert_eq!(
                effects,
                vec![
                    crikey_ui::UiEffect::Query("fixtureCopy".into()),
                    crikey_ui::UiEffect::Execute {
                        owner: PluginId("renderer-fixture".into()),
                        item: ItemId("item-1".into()),
                        action: crikey_core::ActionId("run".into()),
                    },
                ],
            );
        }
    }
}

#[test]
fn ordered_tail_retires_on_navigation_or_editor_owner_replacement() {
    use crikey_ui::navigation::{CompiledNavigation, NavigationStyle};
    for replace_navigation in [false, true] {
        let context = create_launcher_context();
        let mut launcher = ordered_action_launcher();
        let mut view = launcher.frame().unwrap();
        build_launcher_frame(&context, launcher_input(vec![]), &view);
        let frame = build_launcher_frame(
            &context,
            launcher_input(vec![
                egui::Event::Text("Copy".into()),
                key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
                key_press(egui::Key::Enter, egui::Modifiers::NONE),
            ]),
            &view,
        );
        let mut generation = 1;
        let mut effects = Vec::new();
        for command in frame.commands {
            apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
        }
        view = launcher.frame().unwrap();
        if replace_navigation {
            crikey_ui::set_launcher_navigation(
                &context,
                Arc::new(CompiledNavigation::compile(NavigationStyle::Emacs, "{}").unwrap()),
            );
        } else {
            context.memory_mut(|memory| memory.request_focus(egui::Id::new("replacement-editor")));
        }
        let frame = build_launcher_frame(&context, launcher_input(vec![]), &view);
        for command in frame.commands {
            apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
        }
        assert!(effects.is_empty());
    }
}

#[test]
fn recall_ime_owned_return_never_accepts_the_previous_submission() {
    for previous_frame in [false, true] {
        let context = create_launcher_context();
        let mut state = crikey_ui::LauncherViewModel::new();
        state.activate();
        state.set_query_recall(vec!["previous submission".into()]);
        let mut view = state.frame().unwrap();
        let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let preedit = vec![
            egui::Event::Ime(egui::ImeEvent::Enabled),
            egui::Event::Ime(egui::ImeEvent::Preedit("に".into())),
        ];
        let mut events = if previous_frame {
            let frame = build_launcher_frame(&context, launcher_input(preedit), &view);
            for command in frame.commands {
                let _ = state.apply(command);
            }
            view = state.frame().unwrap();
            Vec::new()
        } else {
            preedit
        };
        events.push(key_press(egui::Key::Enter, egui::Modifiers::NONE));
        events.push(egui::Event::Ime(egui::ImeEvent::Commit("日本".into())));
        let frame = build_launcher_frame(&context, launcher_input(events), &view);
        assert!(!frame.commands.contains(&UiCommand::ExecuteDefault));
        for command in frame.commands {
            let _ = state.apply(command);
        }
        assert_eq!(state.frame().unwrap().query, "日本");
    }
}

#[test]
fn back_keeps_the_wheeled_anchor_across_later_provider_publication() {
    let mut state = crikey_ui::LauncherViewModel::new();
    state.activate();
    state.apply(UiCommand::SetQuery("row".into()));
    state.begin_generation(Generation::from_raw(1));
    state.publish(Generation::from_raw(1), (0..40).map(result_row).collect(), false);
    let view = state.frame().unwrap();
    let mut frames = Frames::new();
    let _ = frames.settle(&view);
    let mut wheel = wheel_over_list();
    if let egui::Event::MouseWheel { delta, .. } = &mut wheel[1] {
        delta.y = -1087.0;
    }
    let _ = frames.draw(&view, wheel);
    let scrolled = frames.settle(&view);
    assert!(!painted(&scrolled, "row-0"));
    for command in scrolled.commands {
        let _ = state.apply(command);
    }
    let anchor = state.navigation_restoration().scroll;
    assert_eq!(anchor.row, 20);
    assert!(anchor.offset > 0.0);
    state.open_settings(None);
    let settings = state.frame().unwrap();
    let _ = frames.settle(&settings);
    state.close_settings();
    state.take_query_refresh().unwrap();
    state.begin_generation(Generation::from_raw(2));
    state.publish(Generation::from_raw(2), (0..2).map(result_row).collect(), true);
    let mut back = state.frame().unwrap();
    for paint in 0..40 {
        let partial = frames.draw(&back, Vec::new());
        if paint > 20 {
            assert!(painted(&partial, "row-0"));
        }
        for command in partial.commands {
            let _ = state.apply(command);
        }
        assert_eq!(state.navigation_restoration().scroll, anchor);
        if let Some(next) = state.frame() {
            back = next;
        }
    }
    state.publish(Generation::from_raw(2), (0..40).map(result_row).collect(), false);
    let refreshed = frames.settle(&state.frame().unwrap());
    assert!(
        !painted(&refreshed, "row-0"),
        "a late provider allocation cannot undo restored scroll"
    );
    let refreshed_anchor = refreshed
        .commands
        .iter()
        .find_map(|command| {
            if let UiCommand::SetLogicalScroll(scroll) = command {
                Some(scroll.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(refreshed_anchor.row, anchor.row);
    assert!((refreshed_anchor.offset - anchor.offset).abs() < 1.0);
    state.apply(UiCommand::SelectNext);
    let navigated = frames.settle(&state.frame().unwrap());
    assert!(
        painted(&navigated, "row-1"),
        "an actual navigation intent supersedes viewport restoration"
    );
}

#[test]
fn command_management_preserves_membership_controls_and_submits_exact_shortcut_edits() {
    let mut row = result_row(0);
    row.target = crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
        provider_id: PluginId("renderer-fixture".into()),
        command_id: "exact-command".into(),
    });
    row.command_state = Some(crikey_ui::CommandRowState {
        registry_revision: 4,
        preference_revision: 7,
        available: true,
        unavailable_reason: None,
        favorite: Some(0),
        favorite_count: 2,
        hidden: false,
        fallback: None,
        fallback_count: 0,
        aliases: vec!["stored alias".into()],
        hotkey: Some("stored shortcut".into()),
        argument_policy: crikey_core::ArgumentPolicy::Forbidden,
        argument: None,
        ranking_reset_available: true,
    });
    let target = row.target.clone();
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.begin_generation(Generation::from_raw(1));
    model.publish(Generation::from_raw(1), vec![row], false);
    model.apply(UiCommand::ShowActions);
    let context = create_launcher_context();
    let view = model.frame().unwrap();
    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&frame, "exact-command"));
    for label in [
        "Favorite / Home",
        "Hidden from discovery",
        "Fallback",
        "stored alias",
        "stored shortcut",
    ] {
        let (visible, _) = reveal_action_text(&context, &view, label);
        assert!(painted(&visible, label));
    }

    let mut last_submission = None;
    for (previous, value, expected) in [
        (
            "stored alias",
            "new-alias",
            crikey_ui::CommandPreferenceEdit::SetAlias(Some("new-alias".into())),
        ),
        (
            "stored shortcut",
            "Ctrl+Alt+J",
            crikey_ui::CommandPreferenceEdit::SetHotkey(Some("Ctrl+Alt+J".into())),
        ),
    ] {
        let (_, editor) = reveal_action_text(&context, &view, previous);
        let _ = build_launcher_frame(&context, launcher_input(click_at(editor)), &view);
        let crikey_ui::ActionTarget::Command(reference) = &target else {
            unreachable!()
        };
        let key = crikey_ui::command_edit_key(
            reference,
            matches!(&expected, crikey_ui::CommandPreferenceEdit::SetHotkey(_)),
        );
        let draft_id = egui::Id::new(("crikey-setting-draft", key.as_str()));
        assert_eq!(
            context.memory(|memory| memory.focused()),
            Some(draft_id.with("editor")),
            "click the editor, not the assigned shortcut metadata in the result row"
        );
        let typed = build_launcher_frame(
            &context,
            launcher_input(vec![
                key_press(egui::Key::A, egui::Modifiers::COMMAND),
                egui::Event::Text(value.into()),
            ]),
            &view,
        );
        assert_eq!(
            context.data(|data| data.get_temp::<String>(draft_id)),
            Some(value.into())
        );
        assert!(
            !typed
                .commands
                .iter()
                .any(|command| matches!(command, UiCommand::EditCommandShortcut { .. })),
            "typing edits a draft rather than committing it"
        );
        let submitted = build_launcher_frame(
            &context,
            launcher_input(vec![key_press(egui::Key::Enter, egui::Modifiers::NONE)]),
            &view,
        );
        let request = submitted
            .commands
            .into_iter()
            .find(|command| {
                matches!(command, UiCommand::EditCommandShortcut { target: edited, edit, .. }
                if edited == &target && edit == &expected)
            })
            .expect("committing the focused editor submits its exact command and semantic edit");
        let UiCommand::EditCommandShortcut { request_id, .. } = &request else {
            unreachable!()
        };
        let request_id = *request_id;
        assert_ne!(request_id, 0);
        last_submission = Some(request_id);
        assert!(matches!(model.apply(request),
            Some(crikey_ui::UiEffect::MutateCommandPreferences {
                request_id: admitted, expected_revision: 7, edit, ..
            }) if admitted == request_id && edit == expected));
    }
    let mut frames = Frames {
        context: context.clone(),
        clock: context.input(|input| input.time),
    };
    let before_scroll = frames.settle(&view);
    let anchor = position_of(&before_scroll, "Ctrl+Alt+J");
    let _ = frames.draw(
        &view,
        vec![
            egui::Event::PointerMoved(anchor + egui::vec2(4.0, 4.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -48.0),
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    let scrolled = frames.settle(&view);
    let scrolled_anchor = position_of(&scrolled, "Ctrl+Alt+J");
    assert!(
        scrolled_anchor.y < anchor.y - 1.0,
        "the user actually scrolled the management sheet"
    );
    let crikey_ui::ActionTarget::Command(reference) = &target else {
        unreachable!()
    };
    model.command_edit_completed(crikey_ui::CommandPreferenceEditOutcome {
        request_id: last_submission.unwrap(),
        reference: reference.clone(),
        edit: crikey_ui::CommandPreferenceEdit::SetHotkey(Some("Ctrl+Alt+J".into())),
        status: crikey_ui::SettingEditStatus::Failed,
    });
    model.publish(Generation::from_raw(1), view.rows.to_vec(), false);
    let refreshed = model.frame().unwrap();
    let after_refresh = frames.settle(&refreshed);
    assert!(
        (position_of(&after_refresh, "Ctrl+Alt+J").y - scrolled_anchor.y).abs() < 1.0,
        "unchanged selection across provider refresh and draft ACK preserves manual scroll"
    );
}

#[test]
fn external_settings_reload_never_replaces_an_active_text_draft() {
    let context = create_launcher_context();
    let settings = hotkey_settings_model().settings.to_vec();
    let key = settings[0].key.clone();
    let original = settings[0].value.clone();
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.set_settings(settings.clone());
    model.open_settings(None);
    let view = model.frame().unwrap();
    let opened = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let editor = position_of(&opened, &original);
    let _ = build_launcher_frame(
        &context,
        launcher_input(click_at(editor + egui::vec2(4.0, 4.0))),
        &view,
    );
    let typed = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::Text("draft".into())]),
        &view,
    );
    assert!(!typed
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetSetting { .. })));
    let draft_id = egui::Id::new(("crikey-setting-draft", key.as_str()));
    let draft = context.data(|data| data.get_temp::<String>(draft_id)).unwrap();
    assert_ne!(draft, original);
    let mut reloaded = settings;
    reloaded[0].value = "external configuration value".into();
    model.set_settings(reloaded);
    let fresh = model.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &fresh);
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)),
        Some(draft)
    );
}

#[test]
fn settings_configure_commands_reaches_unavailable_saved_fallback_controls() {
    use crikey_ui::settings_management::Category;
    for category in [Category::All, Category::Shortcuts] {
        let context = create_launcher_context();
        context.style_mut(|style| style.animation_time = 0.0);
        let mut command = result_row(0);
        command.target = crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
            provider_id: PluginId("removed-provider".into()),
            command_id: "saved-fallback".into(),
        });
        command.source = None;
        command.command_state = Some(crikey_ui::CommandRowState {
            registry_revision: 4,
            preference_revision: 7,
            available: false,
            unavailable_reason: Some("Command no longer installed".into()),
            favorite: None,
            favorite_count: 0,
            hidden: true,
            fallback: Some(0),
            fallback_count: 1,
            aliases: Vec::new(),
            hotkey: None,
            argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            argument: None,
            ranking_reset_available: false,
        });
        let mut model = crikey_ui::LauncherViewModel::new();
        model.activate();
        model.set_command_management_rows(vec![command]);
        model.open_settings(None);
        let mut view = model.frame().unwrap();
        Arc::make_mut(&mut view.navigation.settings_management).category = category;
        let opened = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        if category == Category::All {
            let (_, configure) = reveal_settings_text(&context, &view, opened, "Configure Commands", false);
            let _ = build_launcher_frame(
                &context,
                launcher_input(click_at(configure + egui::vec2(4.0, 4.0))),
                &view,
            );
        }
        let listed = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let (_, saved) = reveal_settings_text(&context, &view, listed, "saved-fallback", false);
        let _ = build_launcher_frame(
            &context,
            launcher_input(click_at(saved + egui::vec2(4.0, 4.0))),
            &view,
        );
        let expanded = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
        let (expanded, _) =
            reveal_settings_text(&context, &view, expanded, "Command no longer installed", true);
        assert!(painted(&expanded, "Command no longer installed"));
        let (_, fallback) = reveal_settings_text(&context, &view, expanded, "Fallback", true);
        let clicked = build_launcher_frame(
            &context,
            launcher_input(click_at(fallback + egui::vec2(4.0, 4.0))),
            &view,
        );
        let removal = clicked
            .commands
            .into_iter()
            .find(|command| {
                matches!(
                    command,
                    UiCommand::EditCommandPreference {
                        edit: crikey_ui::CommandPreferenceEdit::Fallback(false),
                        ..
                    }
                )
            })
            .expect("an unavailable saved fallback remains removable");
        assert!(matches!(
            model.apply(removal),
            Some(crikey_ui::UiEffect::MutateCommandPreferences {
                expected_revision: 7,
                ..
            })
        ));
    }
}

#[test]
fn forbidden_command_actions_accept_keyboard_search_and_enter_without_a_click() {
    let context = create_launcher_context();
    let reference = crikey_core::CommandRef {
        provider_id: PluginId("renderer-fixture".into()),
        command_id: "keyboard-command".into(),
    };
    let mut row = result_row(0);
    row.target = crikey_ui::ActionTarget::Command(reference.clone());
    row.command_state = Some(crikey_ui::CommandRowState {
        registry_revision: 4,
        preference_revision: 7,
        available: true,
        unavailable_reason: None,
        favorite: None,
        favorite_count: 0,
        hidden: false,
        fallback: None,
        fallback_count: 0,
        aliases: Vec::new(),
        hotkey: None,
        argument_policy: crikey_core::ArgumentPolicy::Forbidden,
        argument: None,
        ranking_reset_available: true,
    });
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("discovery text".into()));
    model.begin_generation(Generation::from_raw(1));
    model.publish(Generation::from_raw(1), vec![row], false);
    let root = model.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &root);
    model.apply(UiCommand::ShowActions);
    let open = model.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &open);
    let typed = build_launcher_frame(
        &context,
        launcher_input(vec![egui::Event::Text("Invoke".into())]),
        &open,
    );
    assert!(typed
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetActionQuery(query) if query == "Invoke")));
    assert!(!typed
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetQuery(_))));
    for command in typed.commands {
        model.apply(command);
    }
    let filtered = model.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &filtered);
    let submitted = build_launcher_frame(
        &context,
        launcher_input(vec![key_press(egui::Key::Enter, egui::Modifiers::NONE)]),
        &filtered,
    );
    let effects: Vec<_> = submitted
        .commands
        .into_iter()
        .filter_map(|command| model.apply(command))
        .collect();
    assert!(matches!(effects.as_slice(), [crikey_ui::UiEffect::InvokeCommand {
        reference: actual, argument: None, query, registry_revision: 4, ..
    }] if actual == &reference && query == "discovery text"));
}

#[test]
fn ordinary_query_retirement_frees_its_texture_but_fresh_home_preview_remains_admitted() {
    let context = create_launcher_context();
    let mut host = crikey_ui::LauncherViewModel::new();
    host.activate();
    host.apply(UiCommand::SetQuery("ordinary".into()));
    host.begin_generation(Generation::from_raw(1));
    host.publish(Generation::from_raw(1), vec![result_row(0)], false);
    let publish_image = |host: &mut crikey_ui::LauncherViewModel, color: [u8; 4]| {
        let content = Arc::new(crikey_core::PreviewContent {
            image: Some(crikey_core::PreviewImage {
                width: RASTER[0] as u32,
                height: RASTER[1] as u32,
                rgba: color.repeat(RASTER[0] * RASTER[1]),
            }),
            ..Default::default()
        });
        let weak = Arc::downgrade(&content);
        let selection = host.preview_selection().expect("a live admitted selection");
        assert!(host.publish_preview(
            &selection,
            crikey_ui::PreviewStatus::Content(Arc::new(crikey_ui::prepare_preview(content).unwrap(),))
        ));
        weak
    };
    let ordinary_pixels = publish_image(&mut host, [31, 170, 89, 255]);
    let ordinary_view = host.frame().unwrap();
    let opened = preview_frame(&context, &ordinary_view, 720.0);
    let ordinary_texture = raster_texture(&opened);
    let mut ordinary_sampled = Vec::new();
    for shape in &opened.output.shapes {
        textured_rects(&shape.shape, &mut ordinary_sampled);
    }
    assert!(ordinary_sampled
        .iter()
        .any(|(texture, _)| *texture == ordinary_texture));
    drop(ordinary_view);

    assert_eq!(
        host.apply(UiCommand::SetQuery(String::new())),
        Some(crikey_ui::UiEffect::Query(String::new()))
    );
    let retired = host.frame().unwrap();
    assert!(
        retired.rows.is_empty(),
        "clearing retires prior ordinary results, not merely the displayed string"
    );
    assert!(retired.preview.status.is_none());
    let cleared = preview_frame(&context, &retired, 720.0);
    assert!(cleared.output.textures_delta.free.contains(&ordinary_texture));
    assert!(ordinary_pixels.upgrade().is_none());

    let mut home = result_row(0);
    home.target = crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
        provider_id: home.source.as_ref().unwrap().owner.clone(),
        command_id: "home-image".into(),
    });
    home.source.as_mut().unwrap().kind = crikey_ui::ResultSourceKind::DeclaredCatalog { epoch: 7 };
    home.command_state = Some(crikey_ui::CommandRowState {
        registry_revision: 1,
        preference_revision: 1,
        available: true,
        unavailable_reason: None,
        favorite: Some(0),
        favorite_count: 1,
        hidden: false,
        fallback: None,
        fallback_count: 0,
        aliases: Vec::new(),
        hotkey: None,
        argument_policy: crikey_core::ArgumentPolicy::Forbidden,
        argument: None,
        ranking_reset_available: true,
    });
    host.begin_generation(Generation::from_raw(2));
    host.publish(Generation::from_raw(2), vec![home], false);
    let home_pixels = publish_image(&mut host, [170, 31, 89, 255]);
    let home_view = host.frame().unwrap();
    assert!(home_view.query.is_empty());
    let opened_home = preview_frame(&context, &home_view, 720.0);
    let home_texture = raster_texture(&opened_home);
    let stable_home = preview_frame(&context, &home_view, 720.0);
    assert!(!stable_home.output.textures_delta.free.contains(&home_texture));
    let mut sampled = Vec::new();
    for shape in &stable_home.output.shapes {
        textured_rects(&shape.shape, &mut sampled);
    }
    assert!(sampled.iter().any(|(texture, _)| *texture == home_texture));
    drop(home_view);

    host.open_settings(None);
    let covered = host.frame().unwrap();
    let retired_home = preview_frame(&context, &covered, 720.0);
    assert!(retired_home.output.textures_delta.free.contains(&home_texture));
    assert!(home_pixels.upgrade().is_none());
}

fn setting_edit_model() -> crikey_ui::LauncherViewModel {
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.set_settings(vec![SettingRow {
        key: "launcher.command-home".into(),
        label: "Command home".into(),
        value: "favorites-and-recent".into(),
        source: "user-global".into(),
        control: SettingControl::Text,
    }]);
    model.open_settings(None);
    model
}

fn replace_rendered_setting(
    context: &egui::Context,
    view: &ViewModel,
    previous: &str,
    value: &str,
) -> crikey_ui::NativeUiFrame {
    let visible = build_launcher_frame(context, launcher_input(Vec::new()), view);
    let editor = position_of(&visible, previous);
    let _ = build_launcher_frame(
        context,
        launcher_input(click_at(editor + egui::vec2(4.0, 4.0))),
        view,
    );
    build_launcher_frame(
        context,
        launcher_input(vec![
            key_press(egui::Key::A, egui::Modifiers::COMMAND),
            egui::Event::Text(value.into()),
        ]),
        view,
    )
}

fn save_rendered_setting(
    model: &mut crikey_ui::LauncherViewModel,
    context: &egui::Context,
    view: &ViewModel,
    frame: &crikey_ui::NativeUiFrame,
) -> (u64, String, String) {
    let save = position_of(frame, "Save");
    let submitted = build_launcher_frame(
        context,
        launcher_input(click_at(save + egui::vec2(4.0, 4.0))),
        view,
    );
    submitted
        .commands
        .into_iter()
        .filter_map(|command| model.apply(command))
        .find_map(|effect| match effect {
            crikey_ui::UiEffect::SetSetting {
                request_id,
                key,
                value,
            } => Some((request_id, key, value)),
            _ => None,
        })
        .expect("Save submits one correlated setting request")
}

#[test]
fn setting_save_pending_failure_and_same_source_reload_keep_draft_until_retry_acknowledged() {
    let context = create_launcher_context();
    let mut model = setting_edit_model();
    let view = model.frame().unwrap();
    let typed = replace_rendered_setting(&context, &view, "favorites-and-recent", "recent");
    let draft_id = egui::Id::new(("crikey-setting-draft", "launcher.command-home"));
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)).as_deref(),
        Some("recent")
    );
    let (request_id, key, value) = save_rendered_setting(&mut model, &context, &view, &typed);
    assert_eq!(value, "recent");
    let pending = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&pending, "recent"));
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)).as_deref(),
        Some("recent")
    );
    model.setting_edit_completed(crikey_ui::SettingEditOutcome {
        request_id,
        key: key.clone(),
        value: value.clone(),
        status: crikey_ui::SettingEditStatus::Failed,
    });
    let failed = model.frame().unwrap();
    assert_eq!(failed.settings[0].value, "favorites-and-recent");
    let failure = build_launcher_frame(&context, launcher_input(Vec::new()), &failed);
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)).as_deref(),
        Some("recent")
    );
    let (retry_id, retry_key, retry_value) = save_rendered_setting(&mut model, &context, &failed, &failure);
    assert!(retry_id > request_id);
    let mut saved_settings = failed.settings.to_vec();
    saved_settings[0].value = retry_value.clone();
    model.set_settings(saved_settings);
    let same_source = model.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &same_source);
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)).as_deref(),
        Some("recent"),
        "even an effective value equal to the draft is not a save acknowledgment"
    );
    model.setting_edit_completed(crikey_ui::SettingEditOutcome {
        request_id: retry_id,
        key: retry_key,
        value: retry_value,
        status: crikey_ui::SettingEditStatus::Saved,
    });
    let acknowledged = model.frame().unwrap();
    let saved = build_launcher_frame(&context, launcher_input(Vec::new()), &acknowledged);
    assert!(painted(&saved, "recent"));
    assert_eq!(context.data(|data| data.get_temp::<String>(draft_id)), None);
}

#[test]
fn older_setting_success_does_not_clear_edits_made_while_write_was_pending() {
    let context = create_launcher_context();
    let mut model = setting_edit_model();
    let view = model.frame().unwrap();
    let first = replace_rendered_setting(&context, &view, "favorites-and-recent", "recent");
    let (request_id, key, value) = save_rendered_setting(&mut model, &context, &view, &first);
    let _ = replace_rendered_setting(&context, &view, "recent", "favorites");
    let draft_id = egui::Id::new(("crikey-setting-draft", "launcher.command-home"));
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)).as_deref(),
        Some("favorites")
    );
    let mut persisted = view.settings.to_vec();
    persisted[0].value = value.clone();
    model.set_settings(persisted);
    model.setting_edit_completed(crikey_ui::SettingEditOutcome {
        request_id,
        key,
        value,
        status: crikey_ui::SettingEditStatus::Saved,
    });
    let first_saved = model.frame().unwrap();
    let current = build_launcher_frame(&context, launcher_input(Vec::new()), &first_saved);
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)).as_deref(),
        Some("favorites")
    );
    let (next_id, next_key, next_value) = save_rendered_setting(&mut model, &context, &first_saved, &current);
    assert!(next_id > request_id);
    assert_eq!(next_value, "favorites");
    let mut persisted = first_saved.settings.to_vec();
    persisted[0].value = next_value.clone();
    model.set_settings(persisted);
    model.setting_edit_completed(crikey_ui::SettingEditOutcome {
        request_id: next_id,
        key: next_key,
        value: next_value,
        status: crikey_ui::SettingEditStatus::Saved,
    });
    let latest = model.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &latest);
    assert_eq!(context.data(|data| data.get_temp::<String>(draft_id)), None);
}

#[test]
fn setting_acknowledgment_matches_request_value_and_explicit_cancel_discards_only_the_draft() {
    let context = create_launcher_context();
    let mut model = setting_edit_model();
    let view = model.frame().unwrap();
    let typed = replace_rendered_setting(&context, &view, "favorites-and-recent", "recent");
    let (request_id, key, value) = save_rendered_setting(&mut model, &context, &view, &typed);
    model.setting_edit_completed(crikey_ui::SettingEditOutcome {
        request_id,
        key: key.clone(),
        value: "a different submitted value".into(),
        status: crikey_ui::SettingEditStatus::Saved,
    });
    let unrelated = model.frame().unwrap();
    let current = build_launcher_frame(&context, launcher_input(Vec::new()), &unrelated);
    let draft_id = egui::Id::new(("crikey-setting-draft", "launcher.command-home"));
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)).as_deref(),
        Some("recent")
    );
    let cancel = position_of(&current, "Cancel edit");
    let cancelled = build_launcher_frame(
        &context,
        launcher_input(click_at(cancel + egui::vec2(4.0, 4.0))),
        &unrelated,
    );
    assert!(!cancelled
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetSetting { .. })));
    assert_eq!(context.data(|data| data.get_temp::<String>(draft_id)), None);
    assert_eq!(unrelated.settings[0].value, "favorites-and-recent");
    model.setting_edit_completed(crikey_ui::SettingEditOutcome {
        request_id,
        key,
        value,
        status: crikey_ui::SettingEditStatus::Saved,
    });
    let completed = model.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &completed);
    assert_eq!(context.data(|data| data.get_temp::<String>(draft_id)), None);
}

#[test]
fn changing_a_pending_draft_back_to_submitted_text_still_requires_a_new_save() {
    let context = create_launcher_context();
    let mut model = setting_edit_model();
    let view = model.frame().unwrap();
    let typed = replace_rendered_setting(&context, &view, "favorites-and-recent", "recent");
    let (request_id, key, value) = save_rendered_setting(&mut model, &context, &view, &typed);
    let _ = replace_rendered_setting(&context, &view, "recent", "favorites");
    let _ = replace_rendered_setting(&context, &view, "favorites", "recent");
    let mut persisted = view.settings.to_vec();
    persisted[0].value = value.clone();
    model.set_settings(persisted);
    model.setting_edit_completed(crikey_ui::SettingEditOutcome {
        request_id,
        key,
        value,
        status: crikey_ui::SettingEditStatus::Saved,
    });
    let completed = model.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(Vec::new()), &completed);
    let draft_id = egui::Id::new(("crikey-setting-draft", "launcher.command-home"));
    assert_eq!(
        context.data(|data| data.get_temp::<String>(draft_id)).as_deref(),
        Some("recent")
    );
}

#[test]
fn typed_root_section_headers_are_painted_but_never_executable_rows() {
    let context = create_launcher_context();
    let mut view = scrollable_model(2, 1);
    view.navigation.section_headers = vec![
        crikey_ui::SectionHeader {
            row: 0,
            owner: PluginId("renderer-fixture".into()),
            id: crikey_core::result_presentation::SectionId("first".into()),
            label: "First section".into(),
        },
        crikey_ui::SectionHeader {
            row: 1,
            owner: PluginId("renderer-fixture".into()),
            id: crikey_core::result_presentation::SectionId("second".into()),
            label: "Second section".into(),
        },
    ]
    .into();
    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&frame, "First section"));
    assert!(painted(&frame, "Second section"));
    let position = position_of(&frame, "First section");
    let clicked = build_launcher_frame(&context, launcher_input(click_at(position)), &view);
    assert!(!clicked.commands.iter().any(|command| matches!(
        command,
        UiCommand::ActivateAction { .. }
            | UiCommand::ActivateActionless { .. }
            | UiCommand::InvokeCommand { .. }
            | UiCommand::ExecuteDefault
    )));
    assert_eq!(view.rows.len(), 2);
    assert_eq!(view.selected, 1);
}

#[test]
fn actionless_typed_root_control_keeps_none_instead_of_inventing_an_action_id() {
    let context = create_launcher_context();
    let mut view = scrollable_model(1, 0);
    view.navigation.actionless_targets = vec![crikey_core::structured_arguments::OwnedItem {
        owner: PluginId("renderer-fixture".into()),
        item: ItemId("item-0".into()),
    }]
    .into();
    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    let position = position_of(&frame, "Run");
    let clicked = build_launcher_frame(&context, launcher_input(click_at(position)), &view);
    assert!(clicked.commands.iter().any(|command| matches!(command, UiCommand::ActivateActionless { target: crikey_ui::ActionTarget::Result { owner, item } } if owner.0 == "renderer-fixture" && item.0 == "item-0")));
    assert!(!clicked
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::ActivateAction { .. })));
}

#[test]
fn an_unavailable_restored_scope_requires_explicit_all_selection() {
    let context = create_launcher_context();
    let mut view = model("scope");
    view.navigation.selected_scope = Some(crikey_ui::ScopeIdentity {
        owner: PluginId("gone".into()),
        id: crikey_core::result_presentation::ScopeId("private".into()),
        source: crikey_ui::PublicationIdentity::Suggestion {
            generation: Generation::from_raw(1),
            producer: 1,
        },
    });
    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&frame, "Unavailable scope"));
    assert!(!frame
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetScope(None))));
    let position = position_of(&frame, "Unavailable scope");
    let opened = build_launcher_frame(&context, launcher_input(click_at(position)), &view);
    let position = position_of(&opened, "All");
    let chosen = build_launcher_frame(&context, launcher_input(click_at(position)), &view);
    assert!(chosen
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetScope(None))));
}
#[test]
fn remapped_activation_admits_only_its_text_prefix_and_runs_once() {
    use crikey_ui::navigation::{CompiledNavigation, NavigationStyle};
    let mut frames = Frames::new();
    crikey_ui::set_launcher_navigation(
        &frames.context,
        Arc::new(
            CompiledNavigation::compile(
                NavigationStyle::Default,
                r#"{"activate":["F8"],"next":["Alt+J"],"cancel":["F9"]}"#,
            )
            .unwrap(),
        ),
    );
    let view = model("a");
    let initial = frames.settle(&view);
    assert!(painted(&initial, "Alt+J"));
    assert!(painted(&initial, "F9"));
    assert!(!painted(&initial, "Escape"));
    let frame = frames.draw(
        &view,
        vec![
            egui::Event::Text("b".into()),
            egui::Event::Key {
                key: egui::Key::F8,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::Text("trailing".into()),
        ],
    );
    let query = frame
        .commands
        .iter()
        .position(|command| matches!(command, UiCommand::SetQuery(text) if text == "ab"))
        .expect("text prefix admitted");
    let execute = frame
        .commands
        .iter()
        .position(|command| matches!(command, UiCommand::ExecuteDefault))
        .expect("remapped activation executes");
    assert!(query < execute);
    assert_eq!(
        frame
            .commands
            .iter()
            .filter(|command| matches!(command, UiCommand::ExecuteDefault))
            .count(),
        1
    );
    assert!(!frame
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetQuery(text) if text.contains("trailing"))));
    let unbound = frames.draw(
        &view,
        vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );
    assert!(!unbound.commands.contains(&UiCommand::ExecuteDefault));
}

#[test]
fn closed_theme_choice_is_graphically_selectable_at_minimum_width() {
    use crikey_ui::settings_management::{Field, Panel, Request};
    let context = create_launcher_context();
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::OpenSettings);
    model.set_settings_management(Panel {
        view: model.view().map(|(id, _)| id),
        config: 42,
        schema: 7,
        projection_config: 42,
        projection_schema: 7,
        authoritative: true,
        fields: vec![Field {
            key: "launcher.theme".into(),
            label:
                "Theme with a deliberately long descriptive label that must wrap without hiding its selector"
                    .into(),
            description: "Closed choices share the authoritative settings transaction".into(),
            category: "Appearance".into(),
            plugin: None,
            value: "system".into(),
            source: "session".into(),
            restart: false,
            secret: false,
            control: SettingControl::Choice {
                options: &[("system", "System"), ("light", "Light"), ("dark", "Dark")],
            },
            max_bytes: 4096,
            value_available: true,
        }]
        .into(),
        ..Default::default()
    });
    let view = model.frame().unwrap();
    let draw = |events| {
        let mut input = launcher_input(events);
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(480.0, 520.0),
        ));
        build_launcher_frame(&context, input, &view)
    };
    fn visible_control(frame: &crikey_ui::NativeUiFrame, label: &str) -> Option<egui::Pos2> {
        frame.output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                let center = text.pos + text.galley.size() / 2.0;
                shape
                    .clip_rect
                    .contains_rect(egui::Rect::from_min_size(text.pos, text.galley.size()))
                    .then_some(center)
            }
            _ => None,
        })
    }
    draw(Vec::new());
    let mut initial = draw(Vec::new());
    assert!(painted(&initial, "Quit CriKey"));
    let body_pointer = position_of(&initial, "Quit CriKey") - egui::vec2(40.0, 24.0);
    for _ in 0..32 {
        if visible_control(&initial, "System").is_some() {
            break;
        }
        initial = draw(vec![
            egui::Event::PointerMoved(body_pointer),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -80.0),
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }
    // Let ordinary smooth scrolling finish before pressing the moving control.
    for _ in 0..32 {
        initial = draw(Vec::new());
    }
    let selector =
        visible_control(&initial, "System").expect("the actual selector is reachable by body scrolling");
    assert!(selector.x < 480.0 && selector.y < 520.0);
    draw(click_at(selector));
    assert!(
        context.memory(|memory| memory.any_popup_open()),
        "the graphical selector opens a real popup"
    );
    let opened = draw(Vec::new());
    let selected = draw(click_at(
        visible_control(&opened, "Light").expect("real Light option is visible"),
    ));
    assert!(
        !selected.commands.iter().any(|command| matches!(
            command,
            UiCommand::ManageSettings(Request::Edit { .. }) | UiCommand::SetSetting { .. }
        )),
        "a choice is a retained draft until Save"
    );
    let mut selected = selected;
    for _ in 0..32 {
        selected = draw(Vec::new());
    }
    assert!(!context.memory(|memory| memory.any_popup_open()));
    assert!(
        visible_control(&selected, "Light").is_some(),
        "the retained choice actually changed"
    );
    for _ in 0..32 {
        if visible_control(&selected, "Save").is_some() {
            break;
        }
        selected = draw(vec![
            egui::Event::PointerMoved(body_pointer),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -80.0),
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }
    for _ in 0..32 {
        selected = draw(Vec::new());
    }
    let save = visible_control(&selected, "Save").expect("actual managed Save button is reachable");
    let saved = draw(click_at(save));
    assert!(saved.commands.iter().any(|command| matches!(command, UiCommand::ManageSettings(Request::Edit { key, plugin: None, draft, config: 42, schema: 7, .. }) if key == "launcher.theme" && draft == "light")));
    assert!(
        !saved
            .commands
            .iter()
            .any(|command| matches!(command, UiCommand::SetSetting { .. })),
        "authoritative forms never bypass revision-guarded management"
    );
    assert_eq!(
        view.navigation.settings_management.fields[0].value, "system",
        "selection cannot promote the effective session value before persistence"
    );
}

#[test]
fn advanced_navigation_editor_preserves_the_full_advertised_byte_bound() {
    use crikey_ui::settings_management::{Field, Panel, Request};
    let mut frames = Frames::new();
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::OpenSettings);
    model.set_settings_management(Panel {
        view: model.view().map(|(id, _)| id),
        config: 42,
        schema: 7,
        projection_config: 42,
        projection_schema: 7,
        authoritative: true,
        fields: vec![Field {
            key: "launcher.navigation-bindings".into(),
            label: "Navigation bindings (JSON object)".into(),
            description: String::new(),
            category: "Keyboard".into(),
            plugin: None,
            value: "{}".into(),
            source: "default".into(),
            restart: false,
            secret: false,
            control: SettingControl::Text,
            max_bytes: 8192,
            value_available: true,
        }]
        .into(),
        ..Default::default()
    });
    let mut view = model.frame().unwrap();
    view.settings_focus = Some("launcher.navigation-bindings".into());
    frames.settle(&view);
    let mut value = r#"{"next":["Alt+J"]}"#.to_owned();
    value.extend(std::iter::repeat_n(' ', 8192 - value.len()));
    let frame = frames.draw(
        &view,
        vec![
            key_press(egui::Key::A, egui::Modifiers::COMMAND),
            egui::Event::Paste(value.clone()),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ],
    );
    assert!(frame.commands.iter().any(|command| matches!(command, UiCommand::ManageSettings(Request::Edit { key, draft, config: 42, schema: 7, .. }) if key == "launcher.navigation-bindings" && draft == &value)));
    assert!(!frame
        .commands
        .iter()
        .any(|command| matches!(command, UiCommand::SetSetting { .. })));
}

#[test]
fn focused_preview_returns_through_the_applied_toggle_binding() {
    use crikey_ui::navigation::{CompiledNavigation, NavigationStyle};
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    let selection = launcher.preview_selection().unwrap();
    launcher.publish_preview(&selection, crikey_ui::PreviewStatus::Loading);
    launcher.apply(UiCommand::FocusPreview);
    crikey_ui::set_launcher_navigation(
        &context,
        Arc::new(
            CompiledNavigation::compile(NavigationStyle::Default, r#"{"toggle-preview":["F8"]}"#).unwrap(),
        ),
    );
    let (unchanged, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![key_press(egui::Key::F6, egui::Modifiers::NONE)],
    );
    assert!(unchanged.preview.focused);
    assert!(effects.is_empty());
    let (returned, effects) = drive_ordered_frame_input(
        &context,
        &mut launcher,
        unchanged,
        vec![key_press(egui::Key::F8, egui::Modifiers::NONE)],
    );
    assert!(!returned.preview.focused);
    assert_eq!(returned.query, "fixture");
    assert!(effects.is_empty());
    launcher.apply(UiCommand::FocusPreview);
    crikey_ui::set_launcher_navigation(&context, Arc::new(CompiledNavigation::default()));
    let (returned, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![key_press(egui::Key::F6, egui::Modifiers::NONE)],
    );
    assert!(!returned.preview.focused);
    assert!(effects.is_empty());
}

#[test]
fn focused_preview_keeps_page_scrolling_when_toggle_is_remapped_to_page_down() {
    use crikey_ui::navigation::{CompiledNavigation, NavigationStyle};
    let mut view = scrollable_model(2, 0);
    view.preview.status = Some(crikey_ui::PreviewStatus::Content(Arc::new(
        crikey_ui::prepare_preview(Arc::new(crikey_core::PreviewContent {
            text: Some(crikey_core::PreviewText::Plain(format!(
                "PREVIEW_SCROLL_BEGIN\n{}",
                "Long bounded detail line\n".repeat(150)
            ))),
            ..Default::default()
        }))
        .unwrap(),
    )));
    view.preview.focused = true;
    let mut frames = Frames::new();
    crikey_ui::set_launcher_navigation(
        &frames.context,
        Arc::new(
            CompiledNavigation::compile(
                NavigationStyle::Default,
                r#"{"toggle-preview":["PageDown"],"page-down":[]}"#,
            )
            .unwrap(),
        ),
    );
    let initial = frames.settle(&view);
    let before = position_of(&initial, "PREVIEW_SCROLL_BEGIN");
    let page = frames.draw(&view, vec![key_press(egui::Key::PageDown, egui::Modifiers::NONE)]);
    assert!(!page.commands.contains(&UiCommand::LeavePreview));
    let scrolled = frames.settle(&view);
    assert!(position_of(&scrolled, "PREVIEW_SCROLL_BEGIN").y < before.y);
    assert!(!painted(&scrolled, "PageDown"));
}

#[test]
fn focused_preview_keeps_link_traversal_when_cancel_is_remapped_to_tab() {
    use crikey_ui::navigation::{CompiledNavigation, NavigationStyle};
    use crikey_ui::{LauncherViewModel, PreviewLinkTarget, PreviewStatus};
    let mut state = LauncherViewModel::new();
    state.activate();
    state.apply(UiCommand::SetQuery("preview".into()));
    state.begin_generation(Generation::from_raw(1));
    state.publish(Generation::from_raw(1), vec![result_row(0)], false);
    let selection = state.preview_selection().unwrap();
    let target = PreviewLinkTarget {
        selection: selection.clone(),
        plugin: selection.plugin.clone(),
        incarnation: 7,
        request: 9,
    };
    state.publish_preview(
        &selection,
        PreviewStatus::Content(Arc::new(
            crikey_ui::prepare_preview(Arc::new(crikey_core::PreviewContent {
                text: Some(crikey_core::PreviewText::Markdown(
                    "[Document link](https://example.org/)".into(),
                )),
                ..Default::default()
            }))
            .unwrap(),
        )),
    );
    state.set_preview_link_target(target.clone());
    let mut view = state.frame().unwrap();
    view.preview.focused = true;
    let mut frames = Frames::new();
    crikey_ui::set_launcher_navigation(
        &frames.context,
        Arc::new(
            CompiledNavigation::compile(
                NavigationStyle::Default,
                r#"{"cancel":["Tab","Escape"],"complete":[]}"#,
            )
            .unwrap(),
        ),
    );
    frames.settle(&view);
    let focused_before = frames.context.memory(|memory| memory.focused());
    assert!(focused_before.is_some());
    let tab = frames.draw(&view, vec![key_press(egui::Key::Tab, egui::Modifiers::NONE)]);
    assert!(!tab.commands.contains(&UiCommand::LeavePreview));
    assert_ne!(frames.context.memory(|memory| memory.focused()), focused_before);
    let enter = frames.draw(&view, vec![key_press(egui::Key::Enter, egui::Modifiers::NONE)]);
    assert!(enter.commands.contains(&UiCommand::OpenPreviewLink {
        target: Box::new(target),
        url: "https://example.org/".into(),
    }));
    assert!(!enter.commands.contains(&UiCommand::LeavePreview));
}

#[test]
fn applied_size_and_density_change_real_row_layout_without_changing_os_dpi() {
    use crikey_ui::appearance::{
        AppearanceCache, AppearancePreferences, InterfaceSize, ResultDensity, SystemTheme,
    };
    let mut frames = Frames::new();
    let view = scrollable_model(10, 0);
    let default = frames.settle(&view);
    let baseline = position_of(&default, "row-1").y - position_of(&default, "row-0").y;
    let dpi = frames.context.pixels_per_point();
    let cache = AppearanceCache::new(
        AppearancePreferences {
            interface_size: InterfaceSize::Large,
            result_density: ResultDensity::Comfortable,
            ..Default::default()
        },
        SystemTheme::Dark,
    )
    .unwrap();
    crikey_ui::set_launcher_appearance(&frames.context, cache.resolved());
    let large = frames.settle(&view);
    let spacing = position_of(&large, "row-1").y - position_of(&large, "row-0").y;
    assert!(
        spacing > baseline,
        "actual row geometry must respond to both applied preferences"
    );
    assert_eq!(
        frames.context.pixels_per_point(),
        dpi,
        "interface size is not an OS DPI override"
    );
    assert!(painted(&large, "Settings"));
}
#[test]
fn long_applied_remaps_keep_minimum_width_footer_controls_visible_and_clickable() {
    use crikey_ui::appearance::{
        AppearanceCache, AppearancePreferences, InterfaceSize, ResultDensity, SystemTheme, ThemePreference,
    };
    use crikey_ui::navigation::{CompiledNavigation, NavigationStyle};
    for size in [InterfaceSize::Default, InterfaceSize::Large] {
        let context = create_launcher_context();
        let appearance = AppearanceCache::new(
            AppearancePreferences {
                interface_size: size,
                result_density: ResultDensity::Comfortable,
                theme: ThemePreference::Light,
                ..Default::default()
            },
            SystemTheme::Dark,
        )
        .unwrap();
        crikey_ui::set_launcher_appearance(&context, appearance.resolved());
        crikey_ui::set_launcher_navigation(&context, Arc::new(CompiledNavigation::compile(NavigationStyle::Default,
            r#"{"show-actions":["Ctrl+Alt+Shift+Meta+PageDown"],"activate":["Ctrl+Alt+Shift+Meta+PageUp"]}"#).unwrap()));
        let mut view = scrollable_model(1, 0);
        let mut row = result_row(0);
        row.default_action = Some(crikey_core::Action {
            action_id: crikey_core::ActionId("primary-action".into()),
            label: "PrimaryAction with a deliberately long guest label that must visibly ellipsize".into(),
            description: String::new(),
            applicable_categories: vec![],
            icon_reference: None,
            execution_policy: crikey_core::ExecutionPolicy::Plugin,
        });
        view.rows = Arc::from([row]);
        let draw = |events| {
            let mut input = launcher_input(events);
            input.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(480.0, 520.0),
            ));
            build_launcher_frame(&context, input, &view)
        };
        draw(vec![]);
        let frame = draw(vec![]);
        for label in [
            "Settings",
            "Actions",
            "PrimaryAction with a deliberately long guest label that must visibly ellipsize",
        ] {
            let (rect, clip, elided) = frame
                .output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => Some((
                        egui::Rect::from_min_size(text.pos, text.galley.size()),
                        shape.clip_rect,
                        text.galley.elided,
                    )),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("mandatory control {label} must be painted"));
            assert!(
                rect.left() >= 0.0 && rect.right() <= 480.0 && rect.top() >= 0.0 && rect.bottom() <= 520.0,
                "mandatory control {label} exceeded the window: {rect:?}"
            );
            assert!(
                clip.contains_rect(rect),
                "mandatory control {label} was clipped: size={size:?} rect={rect:?} clip={clip:?}"
            );
            if label.starts_with("PrimaryAction") {
                assert!(elided, "the long primary control must visibly ellipsize");
            }
            let clicked = draw(click_at(rect.center()));
            match label {
                "Settings" => assert!(clicked.commands.contains(&UiCommand::OpenSettings)),
                "Actions" => assert!(clicked.commands.contains(&UiCommand::ShowActions), "size={size:?} painted={rect:?} clip={clip:?} commands={:?}", clicked.commands),
                _ => assert!(clicked.commands.iter().any(|command| matches!(command, UiCommand::ActivateAction {
                    target: crikey_ui::ActionTarget::Result { owner, item }, identity: crikey_ui::ActionIdentity::Plugin(action)
                } if owner.0 == "renderer-fixture" && item.0 == "item-0" && action.0 == "primary-action"))),
            }
        }
    }
}

fn pending_scroll_return() -> (crikey_ui::LauncherViewModel, Frames, ViewModel) {
    let mut state = crikey_ui::LauncherViewModel::new();
    state.activate();
    state.apply(UiCommand::SetQuery("row".into()));
    let mut restoration = state.navigation_restoration();
    restoration.scroll = crikey_ui::LogicalScroll { row: 20, offset: 7.0 };
    state.restore_navigation_state(restoration);
    state.begin_generation(Generation::from_raw(1));
    state.publish(Generation::from_raw(1), (0..2).map(result_row).collect(), true);
    let view = state.frame().unwrap();
    let mut frames = Frames::new();
    for _ in 0..40 {
        for command in frames.draw(&view, Vec::new()).commands {
            let _ = state.apply(command);
        }
    }
    assert_eq!(state.navigation_restoration().scroll.row, 20);
    (state, frames, view)
}

#[test]
fn terminal_short_return_clamps_once_without_reviving_the_saved_anchor() {
    let (mut state, mut frames, _) = pending_scroll_return();
    state.publish(Generation::from_raw(1), (0..2).map(result_row).collect(), false);
    let terminal = frames.settle(&state.frame().unwrap());
    for command in terminal.commands {
        let _ = state.apply(command);
    }
    assert_eq!(state.navigation_restoration().scroll.row, 0);
    state.begin_generation(Generation::from_raw(2));
    state.publish(Generation::from_raw(2), (0..40).map(result_row).collect(), false);
    let later = frames.settle(&state.frame().unwrap());
    assert!(painted(&later, "row-0"));
    assert!(!painted(&later, "row-20"));
}

#[test]
fn explicit_input_supersedes_a_partial_return_scroll() {
    for input in ["wheel", "navigation", "query"] {
        let (mut state, mut frames, view) = pending_scroll_return();
        match input {
            "wheel" => {
                let drawn = frames.settle(&view);
                let position = position_of(&drawn, "row-0") + egui::vec2(4.0, 4.0);
                let mut events = wheel_over_list();
                events[0] = egui::Event::PointerMoved(position);
                for command in frames.draw(&view, events).commands {
                    let _ = state.apply(command);
                }
                assert_eq!(
                    state.navigation_restoration().scroll,
                    crikey_ui::LogicalScroll::default(),
                    "a wheel over the short list supersedes the model's saved anchor immediately"
                );
                // egui smooths wheel input across paints, even when this short
                // list cannot scroll. Finish that gesture while the provider
                // is pending rather than asking the later full list to ignore it.
                for command in frames.settle(&view).commands {
                    let _ = state.apply(command);
                }
            }
            "navigation" => {
                state.apply(UiCommand::SelectNext);
            }
            "query" => {
                state.apply(UiCommand::SetQuery("edited row".into()));
            }
            _ => unreachable!(),
        }
        state.begin_generation(Generation::from_raw(2));
        state.publish(Generation::from_raw(2), (0..40).map(result_row).collect(), false);
        let later = frames.settle(&state.frame().unwrap());
        assert!(painted(&later, "row-0"), "{input} supersedes restoration");
        assert!(!painted(&later, "row-20"), "{input} supersedes restoration");
        for command in later.commands {
            let _ = state.apply(command);
        }
        assert_eq!(state.navigation_restoration().scroll.row, 0);
    }
}

#[test]
fn expanded_applied_navigation_uses_readable_columns_at_real_launcher_width() {
    let context = create_launcher_context();
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::OpenSettings);
    let view = model.frame().unwrap();
    let draw = |events| {
        let mut input = launcher_input(events);
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(720.0, 960.0),
        ));
        build_launcher_frame(&context, input, &view)
    };
    draw(vec![]);
    let initial = draw(vec![]);
    let header = position_of(&initial, "Applied navigation shortcuts") + egui::vec2(8.0, 6.0);
    draw(click_at(header));
    let mut expanded = draw(vec![]);
    for _ in 0..32 {
        expanded = draw(vec![]);
    }
    let text = |label: &str| {
        expanded
            .output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => Some((shape, text)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("actual applied binding {label} is visible"))
    };
    let next = text("next");
    let down = text("Down");
    let previous = text("previous");
    let longest_name = text(crikey_ui::navigation::NavigationAction::TogglePreview.name());
    for (shape, text) in [next, down, previous, longest_name] {
        assert_eq!(
            text.galley.rows.len(),
            1,
            "binding cells must not split ordinary words"
        );
        assert!(!text.galley.elided);
        assert!(shape
            .clip_rect
            .contains_rect(egui::Rect::from_min_size(text.pos, text.galley.size())));
    }
    let name_bounds = [next, previous, longest_name]
        .map(|(_, text)| egui::Rect::from_min_size(text.pos, text.galley.size()));
    assert!(
        name_bounds.iter().all(|rect| rect.right() < down.1.pos.x),
        "binding values must start beyond every fully readable name, without column overlap"
    );
}

#[test]
fn ordered_action_text_navigation_and_return_execute_the_correct_filtered_leaf() {
    for text_first in [true, false] {
        let context = create_launcher_context();
        let mut launcher = ordered_action_launcher();
        let text = egui::Event::Text("Copy".into());
        let down = key_press(egui::Key::ArrowDown, egui::Modifiers::NONE);
        let mut events = if text_first {
            vec![text, down]
        } else {
            vec![down, text]
        };
        events.push(key_press(egui::Key::Enter, egui::Modifiers::NONE));
        let (view, effects) = drive_ordered_action_input(&context, &mut launcher, events);
        assert_eq!(view.query, "fixture");
        assert_eq!(
            effects,
            vec![ordered_execution(if text_first { "reveal" } else { "copy" })]
        );
    }
}

#[test]
fn ordered_action_selection_paste_navigation_and_return_use_the_existing_editor() {
    let context = create_launcher_context();
    let mut launcher = ordered_action_launcher();
    launcher.apply(UiCommand::SetActionQuery("replace this".into()));
    let (_, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![
            key_press(egui::Key::A, egui::Modifiers::COMMAND),
            egui::Event::Paste("Copy".into()),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ],
    );
    assert_eq!(effects, vec![ordered_execution("reveal")]);
}

#[test]
fn ordered_action_nested_back_then_filter_and_return_respects_each_back_rung() {
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    launcher.apply(UiCommand::ShowActions);
    let view = launcher.frame().unwrap();
    launcher.apply(UiCommand::ActivateAction {
        target: crikey_ui::ActionTarget::of(&view.rows[0]),
        identity: crikey_ui::ActionIdentity::Submenu("launcher-settings".into()),
    });
    let (view, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![
            egui::Event::Text("Copy".into()),
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            egui::Event::Text("Reveal".into()),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ],
    );
    assert_eq!(view.query, "fixture");
    assert_eq!(effects, vec![ordered_execution("reveal")]);
}

#[test]
fn ordered_action_close_transfers_focus_before_root_text_and_return() {
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    launcher.apply(UiCommand::ShowActions);
    let (view, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            key_press(egui::Key::A, egui::Modifiers::COMMAND),
            egui::Event::Text("replacement".into()),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ],
    );
    assert_eq!(view.query, "replacement");
    assert_eq!(
        effects,
        vec![
            crikey_ui::UiEffect::Query("replacement".into()),
            ordered_execution("run"),
        ]
    );
}

#[test]
fn custom_page_footer_at_two_x_dpi_keeps_both_identity_labels_and_settings_separate() {
    let context = create_launcher_context();
    let mut view = page_view(vec![]);
    let title = "Custom page with a deliberately long title that must ellipsize within its footer slot";
    let provider = "Provider with another deliberately long readable identity";
    let page = view.page.as_mut().unwrap();
    Arc::make_mut(&mut page.frame).title = title.into();
    page.plugin_name = provider.into();
    let draw = |events| {
        let mut input = launcher_input(events);
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(480.0, 520.0),
        ));
        input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(2.0);
        build_launcher_frame(&context, input, &view)
    };
    draw(vec![]);
    let frame = draw(vec![]);
    assert_eq!(context.pixels_per_point(), 2.0);
    let painted = |label: &str| {
        frame
            .output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => Some((shape, text)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("footer identity/control {label} is painted"))
    };
    let title = painted(title);
    let provider = painted(provider);
    let settings = painted("Settings");
    let bounds = |text: &egui::epaint::TextShape| egui::Rect::from_min_size(text.pos, text.galley.size());
    for (shape, text) in [title, provider, settings] {
        let rect = bounds(text);
        assert!(
            shape.clip_rect.contains_rect(rect),
            "text={:?} rect={rect:?} clip={:?}",
            text.galley.text(),
            shape.clip_rect
        );
        assert!(rect.left() >= 0.0 && rect.right() <= 480.0 && rect.bottom() <= 520.0);
    }
    assert!(title.1.galley.elided && provider.1.galley.elided);
    assert!(bounds(title.1).right() < bounds(provider.1).left());
    assert!(bounds(provider.1).right() < bounds(settings.1).left());
    for shape in &frame.output.shapes {
        if let egui::Shape::Text(text) = &shape.shape {
            assert!(
                !matches!(text.galley.text(), "navigate" | "complete" | "cancel"),
                "page ownership cannot advertise root bindings"
            );
        }
    }
    let clicked = draw(click_at(bounds(settings.1).center()));
    assert!(clicked.commands.contains(&UiCommand::OpenSettings));
    assert!(page_inputs(&clicked).is_empty());
}

#[test]
fn narrow_large_applied_map_keeps_names_and_long_bindings_visible_and_really_scrolls() {
    use crikey_ui::appearance::{AppearanceCache, AppearancePreferences, InterfaceSize, SystemTheme};
    use crikey_ui::navigation::{CompiledNavigation, NavigationAction, NavigationStyle};
    let context = create_launcher_context();
    let appearance = AppearanceCache::new(
        AppearancePreferences {
            interface_size: InterfaceSize::Large,
            ..Default::default()
        },
        SystemTheme::Light,
    )
    .unwrap();
    crikey_ui::set_launcher_appearance(&context, appearance.resolved());
    let navigation = CompiledNavigation::compile(
        NavigationStyle::Default,
        r#"{"next":["Ctrl+Alt+Shift+Meta+PageDown"],"previous":["Ctrl+Alt+Shift+Meta+PageUp"]}"#,
    )
    .unwrap();
    let next = navigation
        .bindings()
        .iter()
        .find(|binding| binding.action == NavigationAction::Next)
        .unwrap()
        .hint
        .clone();
    let previous = navigation
        .bindings()
        .iter()
        .find(|binding| binding.action == NavigationAction::Previous)
        .unwrap()
        .hint
        .clone();
    crikey_ui::set_launcher_navigation(&context, Arc::new(navigation));
    let mut model = crikey_ui::LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::OpenSettings);
    let view = model.frame().unwrap();
    let mut time = 0.0;
    let mut draw = |events| {
        time += 0.1;
        let mut input = launcher_input(events);
        input.time = Some(time);
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(480.0, 960.0),
        ));
        build_launcher_frame(&context, input, &view)
    };
    draw(vec![]);
    let initial = draw(vec![]);
    let header = position_of(&initial, "Applied navigation shortcuts") + egui::vec2(8.0, 6.0);
    draw(click_at(header));
    let mut expanded = draw(vec![]);
    for _ in 0..12 {
        expanded = draw(vec![]);
    }
    fn cell<'a>(
        frame: &'a crikey_ui::NativeUiFrame,
        label: &str,
    ) -> (&'a egui::epaint::ClippedShape, &'a egui::epaint::TextShape) {
        frame
            .output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => Some((shape, text)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("actual applied name/binding {label} is painted"))
    }
    for label in ["next", "previous", next.as_str(), previous.as_str()] {
        let (shape, text) = cell(&expanded, label);
        let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
        assert_eq!(text.galley.rows.len(), 1);
        assert!(!text.galley.elided);
        if matches!(label, "next" | "previous") {
            assert!(
                shape.clip_rect.contains_rect(rect),
                "the whole action name must be visible: {label} {rect:?} clip={:?}",
                shape.clip_rect
            );
        } else {
            assert!(
                shape.clip_rect.intersect(rect).width() >= 36.0,
                "a real portion of each binding must be visible before scrolling: {label} {rect:?} clip={:?}",
                shape.clip_rect
            );
        }
    }
    let (shape, text) = cell(&expanded, &next);
    let before = egui::Rect::from_min_size(text.pos, text.galley.size());
    let pointer = shape.clip_rect.intersect(before).center();
    let mut scrolled = draw(vec![
        egui::Event::PointerMoved(pointer),
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(-100.0, 0.0),
            modifiers: egui::Modifiers::NONE,
        },
    ]);
    for _ in 0..12 {
        scrolled = draw(vec![]);
    }
    for label in [next.as_str(), previous.as_str()] {
        let (shape, text) = cell(&scrolled, label);
        let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
        assert!(
            shape.clip_rect.contains_rect(rect),
            "the complete chord is reachable: {label} {rect:?} clip={:?}",
            shape.clip_rect
        );
        assert!(
            rect.left() < before.left() - 1.0,
            "real horizontal input moves the binding column"
        );
    }
}

#[test]
fn ordered_action_close_discovery_filter_and_return_uses_the_new_panel() {
    let physical_command = egui::Modifiers {
        ctrl: !cfg!(target_os = "macos"),
        mac_cmd: cfg!(target_os = "macos"),
        command: true,
        ..egui::Modifiers::NONE
    };
    for (name, discovery) in [
        (
            "logical command",
            key_press(egui::Key::K, egui::Modifiers::COMMAND),
        ),
        ("physical command", key_press(egui::Key::K, physical_command)),
        ("alt enter", key_press(egui::Key::Enter, egui::Modifiers::ALT)),
    ] {
        let context = create_launcher_context();
        let mut launcher = action_launcher();
        launcher.apply(UiCommand::ShowActions);
        let (_, effects) = drive_ordered_action_input(
            &context,
            &mut launcher,
            vec![
                key_press(egui::Key::Escape, egui::Modifiers::NONE),
                discovery,
                egui::Event::Text("Reveal".into()),
                key_press(egui::Key::Enter, egui::Modifiers::NONE),
            ],
        );
        assert_eq!(effects, vec![ordered_execution("reveal")], "{name}");
    }
}

#[test]
fn ordered_action_close_preview_focus_back_and_return_use_the_shared_adapter() {
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    let selection = launcher.preview_selection().unwrap();
    launcher.publish_preview(&selection, crikey_ui::PreviewStatus::Loading);
    launcher.apply(UiCommand::ShowActions);
    let (view, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            key_press(egui::Key::F6, egui::Modifiers::NONE),
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ],
    );
    assert!(!view.preview.focused);
    assert_eq!(effects, vec![ordered_execution("run")]);
}

#[test]
fn ordered_action_close_settings_back_and_return_use_the_shared_adapter() {
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    launcher.apply(UiCommand::ShowActions);
    let (view, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            key_press(egui::Key::Comma, egui::Modifiers::COMMAND),
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ],
    );
    assert!(!view.settings_open);
    assert_eq!(effects.last(), Some(&ordered_execution("run")));
}

#[test]
fn ordered_action_composition_navigation_return_and_commit_never_execute() {
    let context = create_launcher_context();
    let mut launcher = ordered_action_launcher();
    let (_, effects) = drive_ordered_action_input(
        &context,
        &mut launcher,
        vec![
            egui::Event::Ime(egui::ImeEvent::Enabled),
            egui::Event::Ime(egui::ImeEvent::Preedit("Co".into())),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
            egui::Event::Ime(egui::ImeEvent::Commit("Copy".into())),
        ],
    );
    assert!(effects.is_empty());
}

#[test]
fn ordered_action_tail_cannot_cross_generation_focus_or_session_retirement() {
    for retirement in ["generation", "focus", "session", "target", "action"] {
        let context = create_launcher_context();
        let mut launcher = ordered_action_launcher();
        let mut view = launcher.frame().unwrap();
        let _ = build_launcher_frame(&context, launcher_input(vec![]), &view);
        let frame = build_launcher_frame(
            &context,
            launcher_input(vec![
                egui::Event::Text("Copy".into()),
                key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
                key_press(egui::Key::Enter, egui::Modifiers::NONE),
            ]),
            &view,
        );
        let mut generation = 1;
        let mut effects = Vec::new();
        for command in frame.commands {
            apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
        }
        assert!(effects.is_empty());
        view = launcher.frame().unwrap();
        let mut input = launcher_input(vec![]);
        match retirement {
            "generation" => {
                launcher.begin_generation(Generation::from_raw(2));
                launcher.publish(Generation::from_raw(2), view.rows.to_vec(), false);
                view = launcher.frame().unwrap();
            }
            "focus" => {
                input.focused = false;
                input.events.push(egui::Event::WindowFocused(false));
            }
            "session" => {
                launcher.dismiss();
                launcher.activate();
                view = launcher.frame().unwrap();
            }
            "target" | "action" => {
                let mut row = view.rows[0].clone();
                if retirement == "target" {
                    let source = row.source.as_mut().unwrap();
                    source.item = ItemId("replacement".into());
                    row.target = crikey_ui::ActionTarget::Result {
                        owner: source.owner.clone(),
                        item: source.item.clone(),
                    };
                } else {
                    row.alternate_actions
                        .retain(|action| action.action_id.0 != "reveal");
                }
                launcher.publish(view.generation, vec![row], false);
                view = launcher.frame().unwrap();
            }
            _ => unreachable!(),
        }
        let frame = build_launcher_frame(&context, input, &view);
        for command in frame.commands {
            apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
        }
        assert!(effects.is_empty(), "{retirement}");
    }
}

#[test]
fn a_reachable_partial_return_releases_the_original_scroll_intent() {
    let (mut state, mut frames, _) = pending_scroll_return();
    state.publish(Generation::from_raw(1), (0..40).map(result_row).collect(), true);
    let view = state.frame().unwrap();
    let restored = frames.settle(&view);
    for command in restored.commands {
        let _ = state.apply(command);
    }
    let anchor = state.navigation_restoration().scroll;
    assert_eq!(anchor.row, 20);
    assert!((anchor.offset - 7.0).abs() < 1.0);
    let _ = frames.draw(&view, wheel_over_list());
    for command in frames.settle(&view).commands {
        let _ = state.apply(command);
    }
    let wheeled = state.navigation_restoration().scroll;
    assert!(wheeled.row > anchor.row);
    state.publish(Generation::from_raw(1), (0..41).map(result_row).collect(), false);
    let completed = frames.settle(&state.frame().unwrap());
    for command in completed.commands {
        let _ = state.apply(command);
    }
    let completed = state.navigation_restoration().scroll;
    assert_eq!(completed.row, wheeled.row);
    assert!((completed.offset - wheeled.offset).abs() < 1.0);
}

fn assert_empty_return_scroll(pending_plugins: bool) {
    let (mut state, mut frames, _) = pending_scroll_return();
    let expected = if pending_plugins {
        crikey_ui::LogicalScroll { row: 20, offset: 7.0 }
    } else {
        crikey_ui::LogicalScroll::default()
    };
    state.publish(Generation::from_raw(1), Vec::new(), pending_plugins);
    let empty = state.frame().unwrap();
    assert!(empty.rows.is_empty());
    for command in frames.settle(&empty).commands {
        let _ = state.apply(command);
    }
    assert_eq!(state.navigation_restoration().scroll, expected);
    state.open_settings(None);
    let _ = frames.settle(&state.frame().unwrap());
    state.close_settings();
    state.take_query_refresh().unwrap();
    state.begin_generation(Generation::from_raw(2));
    state.publish(Generation::from_raw(2), (0..40).map(result_row).collect(), false);
    let returned = frames.settle(&state.frame().unwrap());
    assert_eq!(painted(&returned, "row-0"), !pending_plugins);
    assert_eq!(painted(&returned, "row-20"), pending_plugins);
    for command in returned.commands {
        let _ = state.apply(command);
    }
    let actual = state.navigation_restoration().scroll;
    assert_eq!(actual.row, expected.row);
    assert!((actual.offset - expected.offset).abs() < 1.0);
}

#[test]
fn terminal_empty_return_clears_the_anchor_before_another_settings_back() {
    assert_empty_return_scroll(false);
}

#[test]
fn pending_empty_return_preserves_the_anchor_across_another_settings_back() {
    assert_empty_return_scroll(true);
}

#[test]
fn ordered_action_retained_root_text_precedes_a_genuinely_newer_return() {
    let context = create_launcher_context();
    let mut launcher = action_launcher();
    launcher.apply(UiCommand::ShowActions);
    let mut view = launcher.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(vec![]), &view);
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            key_press(egui::Key::Escape, egui::Modifiers::NONE),
            key_press(egui::Key::A, egui::Modifiers::COMMAND),
            egui::Event::Text("replacement".into()),
        ]),
        &view,
    );
    let mut generation = 1;
    let mut effects = Vec::new();
    for command in frame.commands {
        apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
    }
    view = launcher.frame().unwrap();
    assert!(view.action_panel.is_none());
    // Native ingress holds newer host keys in this same egui stream while
    // the older suffix is owned; they cannot overtake its text.
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![key_press(egui::Key::Enter, egui::Modifiers::NONE)]),
        &view,
    );
    for command in frame.commands {
        apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
    }
    assert_eq!(launcher.frame().unwrap().query, "replacement");
    assert_eq!(
        effects,
        vec![
            crikey_ui::UiEffect::Query("replacement".into()),
            ordered_execution("run"),
        ]
    );
}

#[test]
fn ordered_action_batch_rejects_a_source_replaced_before_the_host_callback() {
    let context = create_launcher_context();
    let mut launcher = ordered_action_launcher();
    let view = launcher.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(vec![]), &view);
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            egui::Event::Text("Copy".into()),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ]),
        &view,
    );
    let mut row = view.rows[0].clone();
    row.alternate_actions
        .retain(|action| action.action_id.0 != "reveal");
    launcher.publish(view.generation, vec![row], false);
    let mut generation = 1;
    let mut effects = Vec::new();
    for command in frame.commands {
        apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
    }
    let view = launcher.frame().unwrap();
    let frame = build_launcher_frame(&context, launcher_input(vec![]), &view);
    for command in frame.commands {
        apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
    }
    assert!(effects.is_empty());
}

#[test]
fn ordered_action_foreign_finish_cannot_consume_the_validated_batch_token() {
    let context = create_launcher_context();
    let mut launcher = ordered_action_launcher();
    let view = launcher.frame().unwrap();
    let _ = build_launcher_frame(&context, launcher_input(vec![]), &view);
    let mut frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ]),
        &view,
    );
    let Some(crikey_ui::UiEffect::InputBatch {
        continuation: Some(token),
        commands,
    }) = launcher.apply(frame.commands.pop().unwrap())
    else {
        panic!("a real retained tail requires an atomic host batch");
    };
    for command in commands {
        assert!(launcher.apply(command).is_none());
    }
    launcher.finish_input_batch(token + 1);
    assert!(launcher.frame().unwrap().input_receipt.is_none());
    launcher.finish_input_batch(token);
    assert!(launcher.frame().unwrap().input_receipt.is_some());
    launcher.finish_input_batch(token);
    assert!(launcher.frame().is_none(), "the receipt is one-shot");
}

#[test]
fn ordered_action_obsolete_tail_does_not_discard_distinct_fresh_native_keys() {
    for key in [egui::Key::Enter, egui::Key::ArrowDown] {
        let context = create_launcher_context();
        let mut launcher = action_launcher();
        launcher.apply(UiCommand::ShowActions);
        let mut view = launcher.frame().unwrap();
        let _ = build_launcher_frame(&context, launcher_input(vec![]), &view);
        let frame = build_launcher_frame(
            &context,
            launcher_input(vec![
                key_press(egui::Key::Escape, egui::Modifiers::NONE),
                egui::Event::Text("obsolete".into()),
            ]),
            &view,
        );
        let mut generation = 1;
        let mut effects = Vec::new();
        for command in frame.commands {
            apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
        }
        launcher.apply(UiCommand::SetQuery("current".into()));
        launcher.begin_generation(Generation::from_raw(2));
        let mut rows = view.rows.to_vec();
        let mut second = rows[0].clone();
        let source = second.source.as_mut().unwrap();
        source.item = ItemId("second".into());
        second.target = crikey_ui::ActionTarget::Result {
            owner: source.owner.clone(),
            item: source.item.clone(),
        };
        rows.push(second);
        launcher.publish(Generation::from_raw(2), rows, false);
        view = launcher.frame().unwrap();
        generation = 2;
        let frame = build_launcher_frame(
            &context,
            launcher_input(vec![key_press(key, egui::Modifiers::NONE)]),
            &view,
        );
        for command in frame.commands {
            apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
        }
        let current = launcher.frame().unwrap_or(view);
        assert_eq!(current.query, "current");
        if key == egui::Key::Enter {
            assert_eq!(effects, vec![ordered_execution("run")]);
        } else {
            assert_eq!(current.selected, 1);
            assert!(effects.is_empty());
        }
    }
}

#[test]
fn ordered_action_oversized_utf8_query_refuses_submission_before_any_callback() {
    for paste in [false, true] {
        for retained_tail in [false, true] {
            let context = create_launcher_context();
            let mut launcher = ordered_action_launcher();
            // This fits the editor's character limit but exceeds the shared
            // action-query UTF-8 byte limit, without relying on truncation.
            let query = "é".repeat(crikey_ui::action_panel::MAX_ACTION_QUERY_BYTES / 2 + 1);
            let mut events = vec![if paste {
                egui::Event::Paste(query)
            } else {
                egui::Event::Text(query)
            }];
            if retained_tail {
                events.push(key_press(egui::Key::ArrowDown, egui::Modifiers::NONE));
            }
            events.push(key_press(egui::Key::Enter, egui::Modifiers::NONE));
            let (view, mut effects) = drive_ordered_action_input(&context, &mut launcher, events);
            assert!(
                effects.is_empty(),
                "a refused query cannot execute the prior leaf"
            );
            let panel = view.action_panel.as_ref().unwrap();
            assert_eq!(panel.query(), "");
            assert_eq!(
                panel.selected(),
                Some(&crikey_ui::ActionIdentity::Plugin(crikey_core::ActionId(
                    "run".into()
                )))
            );
            assert!(view.rows[0].status.is_some());

            // Once the preserved valid state is visibly back, a separate
            // deliberate Enter is still usable; no sticky refusal flag exists.
            let frame = build_launcher_frame(
                &context,
                launcher_input(vec![key_press(egui::Key::Enter, egui::Modifiers::NONE)]),
                &view,
            );
            let mut generation = 1;
            for command in frame.commands {
                apply_ordered_test_command(&mut launcher, command, &view.rows, &mut generation, &mut effects);
            }
            assert_eq!(effects, vec![ordered_execution("run")]);
        }
    }
}

#[test]
fn ordered_action_results_only_frame_cannot_authorize_atomic_input() {
    let context = create_launcher_context();
    let mut launcher = ordered_action_launcher();
    let mut unowned = launcher.frame().unwrap();
    assert!(unowned.view.is_some());
    unowned.view = None;
    let _ = build_launcher_frame(&context, launcher_input(vec![]), &unowned);
    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![
            egui::Event::Text("Copy".into()),
            key_press(egui::Key::ArrowDown, egui::Modifiers::NONE),
            key_press(egui::Key::Enter, egui::Modifiers::NONE),
        ]),
        &unowned,
    );
    let mut generation = 1;
    let mut effects = Vec::new();
    for command in frame.commands {
        apply_ordered_test_command(
            &mut launcher,
            command,
            &unowned.rows,
            &mut generation,
            &mut effects,
        );
    }
    let owned = launcher.frame().unwrap();
    assert!(owned.view.is_some());
    let frame = build_launcher_frame(&context, launcher_input(vec![]), &owned);
    for command in frame.commands {
        apply_ordered_test_command(&mut launcher, command, &owned.rows, &mut generation, &mut effects);
    }
    assert!(effects.is_empty());
}
