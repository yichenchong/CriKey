//! `crikey dev measure-activation` — warm activation on the shared renderer
//! path (spec 25.1, §31.1; roadmap M1).
//!
//! # What this measures, and what it does not
//!
//! [`NativeLauncherHandle::request_activation`] starts the clock, and the
//! sample closes when the retained surface has presented the first frame of
//! that activation. That span is the *platform-independent* half of warm
//! activation: event-loop wake, session claim, window show and focus, egui
//! frame build, `wgpu` draw and present submission.
//!
//! It deliberately excludes the half that is not shared:
//!
//! - **Hotkey delivery.** Nothing here presses a key. On Windows the OS
//!   delivers `WM_HOTKEY` to the message loop *before* `request_activation` is
//!   reached, and that dispatch is not in any sample below.
//! - **Scanout.** The clock stops after `frame.present()` returns, which is a
//!   CPU-side submission, not photons on a display.
//! - **The compositor.** Under Xvfb there is none; a Windows session has DWM.
//!
//! So a figure from this harness bounds CriKey's own cost on the shared path.
//! It is not a substitute for the Win32 runtime measurement, and the roadmap
//! records it as a proxy rather than as closure of that gate.
//!
//! Cold starts cannot pollute the number: the renderer only opens a sample for
//! an activation requested after the GPU surface became ready, so the very
//! first show is excluded by construction rather than by trimming afterwards.

use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crikey_core::Generation;
use crikey_ui::wgpu;
use crikey_ui::{
    ActivationLatencySnapshot, NativeLauncher, NativeLauncherConfig, NativeLauncherEvent,
    NativeLauncherHandle, ResultRow, ViewModel, ACTIVATION_SAMPLE_CAPACITY,
};

/// Warm activations to perform when `--cycles` is not given.
///
/// Chosen so the retained ring is full and the reported p95 is drawn from a
/// complete window rather than from a partially filled one.
const DEFAULT_CYCLES: usize = ACTIVATION_SAMPLE_CAPACITY;

/// Ceiling on one hide→activate→present cycle.
///
/// Not a performance assertion: it turns a renderer that stops presenting into
/// a named failure instead of a harness that hangs the terminal.
const CYCLE_BUDGET: Duration = Duration::from_secs(10);

/// Interval between polls of an observable the event loop publishes.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// How long one bootstrap activation is given to reach a presented frame
/// before it is retried.
///
/// Short on purpose: before `resumed` has run, an activation is dropped
/// silently by design, so the bootstrap must retry rather than block on a
/// sample that cannot arrive.
const BOOTSTRAP_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// The §25.1 budget this measurement is taken against.
const WARM_ACTIVATION_BUDGET: Duration = Duration::from_millis(30);

/// Usage, built from the constants the command actually uses.
fn usage() -> String {
    format!(
        "\
crikey dev measure-activation - warm activation on the shared renderer path (spec 25.1)

USAGE:
    crikey dev measure-activation [--cycles N] [--present-mode vsync|no-vsync]

OPTIONS:
    --cycles N          Warm hide->activate->present cycles to perform
                        (default: {DEFAULT_CYCLES}, the retained ring's capacity)
    --present-mode M    `vsync` (default) measures what a user experiences.
                        `no-vsync` removes swapchain pacing so the remainder is
                        CriKey's own cost. NOT the shipped configuration: a
                        `no-vsync` figure must never be quoted as the product's
                        warm-activation latency.
    -h, --help          Print this message and measure nothing

Writes one `key=value` line per reported field to stdout. Requires a display:
this drives the real retained window, so it is run under a session or Xvfb.

The span measured starts at `request_activation` and ends when the first frame
of that activation has been presented. `get_current_texture` blocks on
swapchain backpressure INSIDE that span, so a vsync run includes the wait for a
free buffer. It EXCLUDES global-hotkey delivery and display scanout, and is not
a substitute for measuring warm activation on Windows through the Win32 hotkey
path.
"
    )
}

/// What a parsed argument list asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// Drive this many warm cycles at this presentation pacing.
    Run { cycles: usize, vsync: bool },
    /// Print usage and measure nothing.
    Usage,
}

/// Parses `crikey dev measure-activation` arguments.
///
/// Mirrors `dev benchmark`: both `--opt N` and `--opt=N` spellings are
/// accepted, an unrecognized argument is refused rather than ignored, and a
/// repeated option takes its last value.
fn parse_args(args: &[String]) -> Result<Request, String> {
    let mut cycles = DEFAULT_CYCLES;
    let mut vsync = true;
    let mut help = false;
    let mut remaining = args.iter();

    while let Some(arg) = remaining.next() {
        let (option, value) = match arg.as_str() {
            "-h" | "--help" => {
                help = true;
                continue;
            }
            "--cycles" => ("--cycles", required_value(&mut remaining, "--cycles")?),
            "--present-mode" => (
                "--present-mode",
                required_value(&mut remaining, "--present-mode")?,
            ),
            other => {
                if let Some(value) = other.strip_prefix("--cycles=") {
                    ("--cycles", value)
                } else if let Some(value) = other.strip_prefix("--present-mode=") {
                    ("--present-mode", value)
                } else {
                    return Err(format!(
                        "unrecognized `dev measure-activation` argument `{other}`"
                    ));
                }
            }
        };

        match option {
            "--cycles" => {
                cycles = value
                    .parse::<usize>()
                    .map_err(|_| format!("`--cycles` needs a whole number of cycles, got `{value}`"))?;
            }
            _ => {
                vsync = match value {
                    "vsync" => true,
                    "no-vsync" => false,
                    other => {
                        return Err(format!(
                            "`--present-mode` is `vsync` or `no-vsync`, got `{other}`"
                        ))
                    }
                };
            }
        }
    }

    if help {
        return Ok(Request::Usage);
    }
    if cycles == 0 {
        return Err("`--cycles` must be at least 1: zero activations measure nothing".to_owned());
    }
    Ok(Request::Run { cycles, vsync })
}

fn required_value<'a>(remaining: &mut std::slice::Iter<'a, String>, option: &str) -> Result<&'a str, String> {
    let value = remaining
        .next()
        .map(String::as_str)
        .ok_or_else(|| format!("`{option}` needs a value"))?;
    if value.starts_with('-') {
        return Err(format!(
            "`{option}` needs a value, got flag-like argument `{value}`"
        ));
    }
    Ok(value)
}

/// One frame to present. Content is irrelevant to the timing; what matters is
/// that a real view model crosses the same submit/draw path the launcher uses.
fn measurement_frame(generation: Generation) -> ViewModel {
    ViewModel {
        generation,
        query: "measure-activation".to_owned(),
        rows: Arc::from(Vec::<ResultRow>::new()),
        selected: 0,
        pending_plugins: false,
        actions_open: false,
        settings_open: false,
        settings: Arc::default(),
        settings_focus: None,
        show_hints: true,
        rounded_corners: true,
        page: None,
    }
}

/// Drives `cycles` warm hide→activate→present cycles against a live renderer.
///
/// Runs off the event-loop thread: `winit` owns the main thread, so the harness
/// observes the renderer exactly the way the application does — through the
/// handle and the latency ring it publishes. Frames are *not* submitted here;
/// the `Activated` callback on the event loop does that, which is where the
/// application submits its own.
fn drive_cycles(handle: &NativeLauncherHandle, cycles: usize) -> Result<(), String> {
    // The cold activation cannot be timed, so it is spent here proving the
    // surface is warm before the first cycle opens.
    warm_up(handle)?;

    for cycle in 0..cycles {
        let before = handle.activation_latency().total_samples;

        handle
            .request_activation()
            .map_err(|error| format!("cycle {cycle}: activation was refused: {error}"))?;

        if !wait_until(CYCLE_BUDGET, || {
            handle.activation_latency().total_samples > before
        }) {
            return Err(format!(
                "cycle {cycle}: no frame was presented within {budget:?}; the renderer stopped \
                 answering activations",
                budget = CYCLE_BUDGET
            ));
        }

        handle
            .request_hide()
            .map_err(|error| format!("cycle {cycle}: hide was refused: {error}"))?;
        if !wait_until(CYCLE_BUDGET, || !handle.is_visible()) {
            return Err(format!(
                "cycle {cycle}: the window stayed visible for {budget:?} after a hide request",
                budget = CYCLE_BUDGET
            ));
        }
    }

    Ok(())
}

/// Brings the surface up and proves it is warm before any cycle is timed.
///
/// `is_visible()` is not a readiness signal: the handle sets it when the
/// activation is *claimed*, which happens before `resumed` has created the GPU
/// surface. An activation requested in that window is deliberately dropped by
/// the renderer (`requested_at < graphics_ready_at`) and produces no sample, so
/// a measured loop started on `is_visible()` alone would wait for a sample that
/// is never coming.
///
/// The first recorded sample is therefore the readiness handshake: it is the
/// one observable that proves the surface exists, an activation reached it, and
/// a frame was presented. Activations are retried until one lands.
fn warm_up(handle: &NativeLauncherHandle) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < CYCLE_BUDGET {
        let before = handle.activation_latency().total_samples;
        handle
            .request_activation()
            .map_err(|error| format!("the warm-up activation was refused: {error}"))?;

        let landed = wait_until(BOOTSTRAP_RETRY_INTERVAL, || {
            handle.activation_latency().total_samples > before
        });

        handle
            .request_hide()
            .map_err(|error| format!("the warm-up hide was refused: {error}"))?;
        if !wait_until(CYCLE_BUDGET, || !handle.is_visible()) {
            return Err(format!(
                "the window stayed visible for {CYCLE_BUDGET:?} after the warm-up hide"
            ));
        }
        if landed {
            return Ok(());
        }
    }

    Err(format!(
        "no activation reached a presented frame within {CYCLE_BUDGET:?}; is a display available?"
    ))
}

/// Polls `cond` against a deadline, returning whether it became true in time.
///
/// Synchronises on an observable the renderer publishes, never on an elapsed
/// duration, so the harness is as fast as the machine yet a stalled renderer
/// fails with a message instead of hanging.
fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Runs the harness and prints what it measured.
pub fn measure_activation(args: &[String]) -> ExitCode {
    let (cycles, vsync) = match parse_args(args) {
        Ok(Request::Usage) => {
            print!("{usage}", usage = usage());
            return ExitCode::SUCCESS;
        }
        Ok(Request::Run { cycles, vsync }) => (cycles, vsync),
        Err(message) => {
            eprintln!("crikey: {message}\n\n{usage}", usage = usage());
            return ExitCode::from(64); // EX_USAGE
        }
    };

    // The same question `run_launcher` asks, and for the same reason it has to
    // be asked here: whether the desktop composites decides the surface's
    // alpha mode, and this harness exists to measure the path the application
    // pays for. A window built opaque while the launcher's is composited would
    // report a presentation cost nobody is charged.
    let config = NativeLauncherConfig {
        composited: crikey_app::App::new().desktop_composites(),
        present_mode: if vsync {
            wgpu::PresentMode::AutoVsync
        } else {
            wgpu::PresentMode::AutoNoVsync
        },
        ..NativeLauncherConfig::default()
    };
    let launcher = match NativeLauncher::new(config) {
        Ok(launcher) => launcher,
        Err(error) => {
            eprintln!("crikey: cannot create the launcher window: {error}");
            return ExitCode::from(70); // EX_SOFTWARE
        }
    };
    let handle = launcher.handle();

    // The driver owns the cycles; the main thread must stay on the event loop
    // because that is where `winit` requires it. The driver's verdict comes back
    // over a channel so the exit status reflects what actually happened.
    let (report, receive_report) = mpsc::channel();
    let driver_handle = handle.clone();
    let driver = thread::spawn(move || {
        let outcome = drive_cycles(&driver_handle, cycles);
        let _ = report.send(outcome);
        let _ = driver_handle.request_exit();
    });

    // The frame is submitted from the `Activated` callback, exactly where
    // `run_native_launcher` submits its own: the callback runs on the event
    // loop immediately before the renderer accepts the pending frame, so the
    // measured path includes the same callback hop the application pays.
    let frame_handle = handle.clone();
    if let Err(error) = launcher.run(move |event| {
        if matches!(event, NativeLauncherEvent::Activated) {
            let _ = frame_handle.submit_frame(&measurement_frame(Generation::ZERO));
        }
    }) {
        eprintln!("crikey: the renderer failed: {error}");
        return ExitCode::from(70); // EX_SOFTWARE
    }

    // The loop has returned, so the driver is finishing or already has.
    let outcome = match receive_report.recv_timeout(CYCLE_BUDGET) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => Err("the driver thread did not report a result".to_owned()),
        Err(RecvTimeoutError::Disconnected) => {
            Err("the driver thread ended without reporting a result".to_owned())
        }
    };
    let _ = driver.join();

    let snapshot = handle.activation_latency();
    print!("{lines}", lines = report_lines(cycles, vsync, &snapshot));

    if let Err(message) = outcome {
        eprintln!("crikey: the harness did not complete the requested cycles: {message}");
        return ExitCode::from(70); // EX_SOFTWARE
    }

    match verdict(cycles, &snapshot) {
        None => ExitCode::SUCCESS,
        Some(reason) => {
            eprintln!("crikey: {reason}");
            ExitCode::from(70) // EX_SOFTWARE
        }
    }
}

/// The measurement as `key=value` lines, one per field.
///
/// The snapshot is destructured rather than read field by field, so a field
/// added to [`ActivationLatencySnapshot`] stops this compiling instead of
/// quietly going unprinted. `measured_span` is emitted because a latency figure
/// without its endpoints invites exactly the misreading this command's module
/// documentation exists to prevent.
fn report_lines(requested_cycles: usize, vsync: bool, snapshot: &ActivationLatencySnapshot) -> String {
    let ActivationLatencySnapshot {
        total_samples,
        retained_samples,
        latest,
        p95,
    } = snapshot;

    let mut lines = String::new();
    lines.push_str("measured_span=request_activation..first_present\n");
    // `AutoNoVsync` is a REQUEST: wgpu falls back Immediate -> Mailbox -> Fifo,
    // so a surface that offers only Fifo silently stays vsynced. The label says
    // `requested` because this harness cannot promise the request was honoured;
    // comparing a vsync and a no-vsync run is what settles it.
    lines.push_str(if vsync {
        "present_mode_requested=vsync\n"
    } else {
        "present_mode_requested=no-vsync\nnot_product_configuration=true\n"
    });
    lines.push_str("excludes=hotkey_delivery,scanout\n");
    lines.push_str(&format!("requested_cycles={requested_cycles}\n"));
    lines.push_str(&format!("warm_samples_total={total_samples}\n"));
    lines.push_str(&format!("retained_samples={retained_samples}\n"));
    lines.push_str(&format!(
        "budget_nanos={budget}\n",
        budget = WARM_ACTIVATION_BUDGET.as_nanos()
    ));
    match p95 {
        Some(p95) => lines.push_str(&format!("p95_nanos={nanos}\n", nanos = p95.as_nanos())),
        None => lines.push_str("p95_nanos=none\n"),
    }
    match latest {
        Some(latest) => lines.push_str(&format!("latest_nanos={nanos}\n", nanos = latest.as_nanos())),
        None => lines.push_str("latest_nanos=none\n"),
    }
    lines
}

/// Why this run does not stand as evidence, if it does not.
///
/// A run that produced fewer warm samples than cycles requested measured
/// something other than what was asked for, and a p95 over the budget is a
/// failure of the §25.1 target rather than of the harness. Both are reported as
/// non-zero exits so a scripted run cannot record a pass it did not earn.
fn verdict(requested_cycles: usize, snapshot: &ActivationLatencySnapshot) -> Option<String> {
    let requested_samples = u64::try_from(requested_cycles).unwrap_or(u64::MAX);
    if snapshot.total_samples < requested_samples {
        return Some(format!(
            "only {taken} warm samples were recorded for {requested_cycles} requested cycles",
            taken = snapshot.total_samples
        ));
    }
    match snapshot.p95 {
        None => Some("no warm activation was measured".to_owned()),
        Some(p95) if p95 > WARM_ACTIVATION_BUDGET => Some(format!(
            "warm activation p95 is {p95:?}, over the {WARM_ACTIVATION_BUDGET:?} budget (spec 25.1)"
        )),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_are_configurable_in_both_spellings() {
        assert_eq!(
            parse_args(&["--cycles".to_owned(), "8".to_owned()]),
            Ok(Request::Run {
                cycles: 8,
                vsync: true
            })
        );
        assert_eq!(
            parse_args(&["--cycles=8".to_owned()]),
            Ok(Request::Run {
                cycles: 8,
                vsync: true
            })
        );
    }

    #[test]
    fn the_present_mode_is_configurable_in_both_spellings() {
        assert_eq!(
            parse_args(&["--present-mode".to_owned(), "no-vsync".to_owned()]),
            Ok(Request::Run {
                cycles: DEFAULT_CYCLES,
                vsync: false
            })
        );
        assert_eq!(
            parse_args(&["--present-mode=vsync".to_owned()]),
            Ok(Request::Run {
                cycles: DEFAULT_CYCLES,
                vsync: true
            })
        );
    }

    /// The shipped configuration is vsynced, so that is what an unqualified run
    /// measures. A harness that silently removed pacing would report a latency
    /// no user ever experiences.
    #[test]
    fn vsync_is_the_default_pacing() {
        assert_eq!(
            parse_args(&[]),
            Ok(Request::Run {
                cycles: DEFAULT_CYCLES,
                vsync: true
            })
        );
    }

    #[test]
    fn an_unknown_present_mode_is_refused() {
        assert!(parse_args(&["--present-mode=fifo".to_owned()]).is_err());
    }

    /// A no-vsync figure must be self-labelling: quoted without its caveat it
    /// reads as the product's warm-activation latency, which it is not.
    #[test]
    fn a_no_vsync_report_marks_itself_as_not_the_product_configuration() {
        let snapshot = ActivationLatencySnapshot {
            total_samples: 1,
            retained_samples: 1,
            latest: Some(Duration::from_millis(2)),
            p95: Some(Duration::from_millis(2)),
        };
        let lines = report_lines(1, false, &snapshot);
        assert!(lines.contains("present_mode_requested=no-vsync"));
        assert!(lines.contains("not_product_configuration=true"));
    }

    #[test]
    fn the_default_fills_the_retained_ring() {
        assert_eq!(
            parse_args(&[]),
            Ok(Request::Run {
                cycles: ACTIVATION_SAMPLE_CAPACITY,
                vsync: true
            })
        );
    }

    #[test]
    fn zero_cycles_is_refused_rather_than_measuring_nothing() {
        assert!(parse_args(&["--cycles=0".to_owned()]).is_err());
    }

    #[test]
    fn an_unrecognized_argument_is_refused_rather_than_ignored() {
        assert!(parse_args(&["--items=5".to_owned()]).is_err());
    }

    /// A short run must not be reported as a pass: the ring would hold fewer
    /// samples than the caller asked for and its p95 would describe a different
    /// workload.
    #[test]
    fn fewer_samples_than_cycles_is_not_a_pass() {
        let snapshot = ActivationLatencySnapshot {
            total_samples: 3,
            retained_samples: 3,
            latest: Some(Duration::from_millis(1)),
            p95: Some(Duration::from_millis(1)),
        };
        assert!(verdict(10, &snapshot).is_some());
    }

    #[test]
    fn a_p95_over_the_budget_fails_the_run() {
        let snapshot = ActivationLatencySnapshot {
            total_samples: 10,
            retained_samples: 10,
            latest: Some(Duration::from_millis(31)),
            p95: Some(WARM_ACTIVATION_BUDGET + Duration::from_millis(1)),
        };
        assert!(verdict(10, &snapshot).is_some());
    }

    #[test]
    fn a_complete_run_inside_the_budget_passes() {
        let snapshot = ActivationLatencySnapshot {
            total_samples: 10,
            retained_samples: 10,
            latest: Some(Duration::from_millis(2)),
            p95: Some(Duration::from_millis(2)),
        };
        assert_eq!(verdict(10, &snapshot), None);
    }

    /// The endpoints are part of the record: a bare percentile is exactly what
    /// gets misread as end-to-end warm activation including the hotkey.
    #[test]
    fn the_report_states_what_the_span_excludes() {
        let snapshot = ActivationLatencySnapshot {
            total_samples: 1,
            retained_samples: 1,
            latest: Some(Duration::from_millis(2)),
            p95: Some(Duration::from_millis(2)),
        };
        let lines = report_lines(1, true, &snapshot);
        assert!(lines.contains("measured_span=request_activation..first_present"));
        assert!(lines.contains("excludes=hotkey_delivery,scanout"));
    }

    #[test]
    fn help_does_not_hide_unknown_activation_options() {
        let args = vec!["--help".to_owned(), "--unknown".to_owned()];
        assert!(parse_args(&args).is_err());
    }
    #[test]
    fn separate_option_values_cannot_consume_the_next_flag() {
        assert!(parse_args(&["--cycles".to_owned(), "--present-mode".to_owned()]).is_err());
        assert!(parse_args(&["--present-mode".to_owned(), "--help".to_owned()]).is_err());
    }
}
