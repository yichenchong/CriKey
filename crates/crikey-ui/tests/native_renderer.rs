use std::{sync::Arc, time::Duration};

use crikey_core::{Generation, ItemId};
use crikey_platform::IconImage;
use crikey_ui::{
    build_launcher_frame, create_launcher_context, egui, ActivationLatencyTracker, NativeLauncher,
    NativeLauncherConfig, NativeLauncherHandle, RendererError, ResultRow, ViewModel,
    ACTIVATION_SAMPLE_CAPACITY,
};

fn model(query: &str) -> ViewModel {
    ViewModel {
        generation: Generation::ZERO,
        query: query.to_owned(),
        rows: Arc::default(),
        selected: 0,
        pending_plugins: false,
        actions_open: false,
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
