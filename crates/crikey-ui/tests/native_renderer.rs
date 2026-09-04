use std::{sync::Arc, time::Duration};

use crikey_core::{
    Generation, ItemId, NodeRole, NodeShape, PageColor, PageFrame, PageImage, PageInput, PageInputKind,
    PageNode, PluginId,
};
use crikey_platform::IconImage;
use crikey_ui::{
    build_launcher_frame, create_launcher_context, egui, ActivationLatencyTracker, NativeLauncher,
    NativeLauncherConfig, NativeLauncherHandle, PageSurface, RendererError, ResultRow, SettingControl,
    SettingRow, UiCommand, ViewModel, ACTIVATION_SAMPLE_CAPACITY,
};

fn model(query: &str) -> ViewModel {
    ViewModel {
        generation: Generation::ZERO,
        query: query.to_owned(),
        rows: Arc::default(),
        selected: 0,
        pending_plugins: false,
        actions_open: false,
        settings_open: false,
        settings: Arc::default(),
        settings_focus: None,
        page: None,
        show_hints: true,
        rounded_corners: true,
    }
}

fn result_row(index: usize) -> ResultRow {
    ResultRow {
        item: ItemId(format!("item-{index}")),
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

fn scrollable_model(rows: usize, selected: usize) -> ViewModel {
    let mut view = model("row");
    view.rows = (0..rows).map(result_row).collect();
    view.selected = selected;
    view
}

#[test]
fn an_empty_query_renders_nothing_but_the_query_field() {
    let context = create_launcher_context();
    let mut view = model("");
    // Rows are handed in deliberately: the renderer must draw no result area
    // for an untyped launcher whatever it is holding.
    view.rows = (0..3).map(result_row).collect();

    let frame = build_launcher_frame(&context, launcher_input(Vec::new()), &view);

    for absent in ["Ready", "Type a name", "No matches", "row-0", "3 results"] {
        assert!(
            !painted(&frame, absent),
            "an untyped launcher shows nothing but the query field, yet it painted {absent:?}"
        );
    }
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
fn a_replaced_list_puts_the_selected_row_back_on_screen() {
    let mut frames = Frames::new();
    let view = scrollable_model(40, 0);
    let _ = frames.draw(&view, Vec::new());
    let _ = frames.draw(&view, wheel_over_list());
    let scrolled = frames.settle(&view);
    assert!(!painted(&scrolled, "row-0"));

    // A republish hands over a different row set with the same selection.
    // Nobody asked for the offset the old list was left at, so the selected
    // row is fetched back into view.
    let replaced = scrollable_model(40, 0);
    let followed = frames.settle(&replaced);

    assert!(
        painted(&followed, "row-0"),
        "a list that was replaced under the selection must show the selected row"
    );
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

    let frame = build_launcher_frame(&context, launcher_input(shortcut), &model(""));

    assert!(frame.commands.contains(&UiCommand::OpenSettings));
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
    const HINTS: &str = "Up/Down navigate   Tab complete   Esc cancel";
    let context = create_launcher_context();

    let shown = build_launcher_frame(&context, launcher_input(Vec::new()), &model(""));
    assert!(
        painted(&shown, HINTS),
        "the launcher shows the hints by default, or this tests nothing"
    );

    let mut hidden_view = model("");
    hidden_view.show_hints = false;
    let hidden = build_launcher_frame(&context, launcher_input(Vec::new()), &hidden_view);
    assert!(
        !painted(&hidden, HINTS),
        "a user who turned the hints off must not be shown them anyway"
    );
}

/// The one thing hiding the hints must never hide.
///
/// `Settings  Ctrl+,` sits in the same footer row and is the only mouse route
/// into the settings surface: if it went with the hints, the setting that hid
/// them could only be undone from a terminal, by a user who no longer has
/// anything on screen telling them the surface exists.
#[test]
fn the_settings_control_survives_a_footer_with_no_hints() {
    let context = create_launcher_context();
    let mut view = model("");
    view.show_hints = false;

    let located = build_launcher_frame(&context, launcher_input(Vec::new()), &view);
    assert!(painted(&located, "Settings  Ctrl+,"));
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
    assert!(painted(&with_hints, "Settings  Ctrl+,"));
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
    let committed = build_launcher_frame(&context, launcher_input(typed), &view);

    let edit = committed
        .commands
        .iter()
        .find_map(|command| match command {
            UiCommand::SetSetting { key, value } => Some((key.as_str(), value.as_str())),
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

    // The switch sits at the right of its row, where the editor used to be.
    let label = position_of(&drawn, "Show keyboard hints");
    let switch = egui::pos2(
        NativeLauncherConfig::default().width as f32 - theme_margin() - 8.0,
        label.y + 8.0,
    );
    let clicked = build_launcher_frame(&context, launcher_input(click_at(switch)), &view);

    let committed = clicked
        .commands
        .iter()
        .find_map(|command| match command {
            UiCommand::SetSetting { key, value } => Some((key.as_str(), value.as_str())),
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
    let label_off = position_of(&drawn_off, "Show keyboard hints");
    let switch_off = egui::pos2(
        NativeLauncherConfig::default().width as f32 - theme_margin() - 8.0,
        label_off.y + 8.0,
    );
    let clicked_off = build_launcher_frame(&context, launcher_input(click_at(switch_off)), &off);
    let committed_off = clicked_off
        .commands
        .iter()
        .find_map(|command| match command {
            UiCommand::SetSetting { key, value } => Some((key.as_str(), value.as_str())),
            _ => None,
        })
        .expect("moving the switch back commits the setting");
    assert_eq!(
        committed_off,
        ("launcher.show-hints", "true"),
        "an off switch turns on"
    );
}

/// The panel's own margin plus the sheet's, which is where the right-hand
/// controls of a settings row end up. Written here rather than exported from
/// the theme, because a test that reached into the theme for it would agree
/// with the renderer by construction.
fn theme_margin() -> f32 {
    12.0 + 12.0
}

/// The sheet's legend tells the truth about both controls it draws.
///
/// The switch and the text field commit differently, so the footer that used
/// to read "Enter or Save commits an edit" was telling a user of a switch to
/// press a key that does nothing. Asserted on the rendered sheet rather than
/// on the sentence alone, because a legend nothing paints is not a legend.
#[test]
fn the_settings_sheet_says_how_each_control_commits() {
    fn sheet(controls: &[SettingControl]) -> ViewModel {
        let mut view = model("");
        view.settings_open = true;
        view.settings = Arc::from(
            controls
                .iter()
                .enumerate()
                .map(|(index, control)| SettingRow {
                    key: format!("launcher.thing-{index}"),
                    label: format!("Thing {index}"),
                    value: "value".to_owned(),
                    source: "default".to_owned(),
                    control: *control,
                })
                .collect::<Vec<_>>(),
        );
        view
    }

    let context = create_launcher_context();

    let switches = build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &sheet(&[SettingControl::Toggle { on: true }]),
    );
    assert!(painted(&switches, "Switches apply at once"), "switches say so");
    assert!(
        !painted(&switches, "Enter or Save"),
        "a sheet with no text field must not send the user after Enter"
    );

    let editors = build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &sheet(&[SettingControl::Text]),
    );
    assert!(painted(&editors, "Enter or Save"), "editors say so");
    assert!(
        !painted(&editors, "Switches"),
        "a sheet with no switch must not explain switches"
    );

    // What the shipped launcher actually presents.
    let both = build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &sheet(&[SettingControl::Text, SettingControl::Toggle { on: false }]),
    );
    assert!(
        painted(&both, "Switches apply at once"),
        "a mixed sheet says both"
    );
    assert!(painted(&both, "Enter or Save"), "a mixed sheet says both");
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
        // `theme::palette().surface`
        egui::Color32::from_rgb(34, 38, 45),
        // `theme::palette().canvas`
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
    let mut view = model("");
    let mut all = vec![probe_node()];
    all.extend(nodes);
    let frame = PageFrame {
        generation: 1,
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
    let view = page_view(vec![first, second, third]);

    let mut reached = Vec::new();
    for events in [
        Vec::new(),
        vec![key_press(egui::Key::Tab, egui::Modifiers::NONE)],
        vec![key_press(egui::Key::Tab, egui::Modifiers::NONE)],
        vec![key_press(egui::Key::Tab, egui::Modifiers::NONE)],
        vec![key_press(egui::Key::Tab, egui::Modifiers::SHIFT)],
    ] {
        let frame = build_launcher_frame(&context, launcher_input(events), &view);
        reached.extend(
            events_of_kind(&frame, PageInputKind::FocusChanged)
                .into_iter()
                .map(|input| input.node_id),
        );
        assert!(
            events_of_kind(&frame, PageInputKind::KeyPressed).is_empty(),
            "Tab is the host's key and must never be forwarded to the plugin"
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
    let view = page_view(vec![page_control(NodeRole::Button, 1, 0.0, 0.0, 60.0, 20.0)]);

    let frame = build_launcher_frame(
        &context,
        launcher_input(vec![key_press(egui::Key::Escape, egui::Modifiers::NONE)]),
        &view,
    );

    assert!(
        frame.commands.contains(&UiCommand::ClosePage),
        "Escape must close the page"
    );
    assert!(
        !page_inputs(&frame).iter().any(|input| input.key == "Escape"),
        "Escape must not be reported to the plugin as a keystroke"
    );
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
    let second = build_launcher_frame(
        &context,
        launcher_input(Vec::new()),
        &page_view(vec![image_node(0.0, 8.0, 30.0, 20.0, colour)]),
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
