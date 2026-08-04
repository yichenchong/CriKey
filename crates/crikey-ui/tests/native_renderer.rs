use std::{sync::Arc, time::Duration};

use crikey_core::{Generation, ItemId};
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
