//! Session-aware capability reporting for the Linux backend (spec 18.2, 18.6).
//!
//! Two halves of one contract. First, which session CriKey is actually in:
//! [`detect_desktop_environment`] reads the environment through an injected
//! getter, never the ambient process environment, so these tests are
//! deterministic and full-suite safe under any CI runner -- X11, Wayland or no
//! display at all.
//!
//! Second, what the backend then claims. Spec 18.2 requires a backend to
//! distinguish `Available`, `Unavailable`, `PermissionGated`, `Partial` and
//! `UnsupportedDesktopEnvironment`, and spec 18.6 makes window control optional
//! on Linux. A blanket answer therefore fails the specification twice over: a
//! Wayland session must be told that global hotkeys and window control are not
//! offered by the *session* (`UnsupportedDesktopEnvironment`), not that CriKey
//! lacks them (`Unavailable`), because only the first tells a plugin author
//! that the missing feature is not a CriKey bug and not a permission prompt
//! away.
//!
//! Deliberate non-goals: no test here grabs a key, opens a display or talks to
//! a compositor. This is the reporting surface only; the X11 grab path is
//! pinned by the hotkey tests.

#![cfg(target_os = "linux")]

use std::collections::HashMap;

use crikey_platform::{Capability, CapabilityState};
use crikey_platform_linux::{detect_desktop_environment, DesktopEnvironment, LinuxBackend};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A fixture environment. Every detection test reads through one of these and
/// never through `std::env`, so a test cannot pass or fail because of the
/// session the suite happens to run in.
fn fixture(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    move |key: &str| map.get(key).cloned()
}

/// Every [`Capability`] variant, listed once, in declaration order.
const ALL_CAPABILITIES: [Capability; 13] = [
    Capability::ApplicationDiscovery,
    Capability::FileSearch,
    Capability::Clipboard,
    Capability::GlobalHotkeys,
    Capability::ProcessLaunch,
    Capability::UriOpen,
    Capability::WindowEnumeration,
    Capability::WindowActivation,
    Capability::Notifications,
    Capability::Icons,
    Capability::FileWatching,
    Capability::SecretStorage,
    Capability::ShellIntegration,
];

/// The three sessions the backend must answer for.
const ALL_ENVIRONMENTS: [DesktopEnvironment; 3] = [
    DesktopEnvironment::X11,
    DesktopEnvironment::Wayland,
    DesktopEnvironment::Headless,
];

/// Position of a capability in [`ALL_CAPABILITIES`].
///
/// The match is exhaustive and carries no wildcard arm on purpose: adding a
/// variant to `Capability` stops this test crate compiling until somebody
/// decides what Linux reports for it, which is the whole point of the
/// exhaustiveness guard below.
fn index_of(capability: Capability) -> usize {
    match capability {
        Capability::ApplicationDiscovery => 0,
        Capability::FileSearch => 1,
        Capability::Clipboard => 2,
        Capability::GlobalHotkeys => 3,
        Capability::ProcessLaunch => 4,
        Capability::UriOpen => 5,
        Capability::WindowEnumeration => 6,
        Capability::WindowActivation => 7,
        Capability::Notifications => 8,
        Capability::Icons => 9,
        Capability::FileWatching => 10,
        Capability::SecretStorage => 11,
        Capability::ShellIntegration => 12,
    }
}

/// The state the Linux backend is required to report, capability by capability
/// and session by session. This is the contract table, written out rather than
/// derived, so that an implementation cannot satisfy it by construction.
fn required_state(environment: DesktopEnvironment, capability: Capability) -> CapabilityState {
    match capability {
        // No display server needed: honest everywhere.
        Capability::ApplicationDiscovery | Capability::ProcessLaunch => CapabilityState::Available,
        // Global shortcuts: optional on Linux (spec 18.6). `GrabKey` is core X
        // protocol, so an X11 session delivers them whether or not anything
        // else is running on the display.
        Capability::GlobalHotkeys => match environment {
            DesktopEnvironment::X11 => CapabilityState::Available,
            DesktopEnvironment::Wayland => CapabilityState::UnsupportedDesktopEnvironment,
            DesktopEnvironment::Headless => CapabilityState::Unavailable,
        },
        // Window control: also optional (spec 18.6), but it needs an EWMH
        // *window manager* on top of the session, and reporting is a pure
        // function of the session that must not open a display to find out.
        // `Partial` is the honest answer for "the session type supports it,
        // subject to a runtime gate": on a bare X server
        // `LinuxBackend::window_service` hands out nothing, so `Available`
        // would be a claim the backend cannot always deliver (spec 18.2).
        Capability::WindowEnumeration | Capability::WindowActivation => match environment {
            DesktopEnvironment::X11 => CapabilityState::Partial,
            DesktopEnvironment::Wayland => CapabilityState::UnsupportedDesktopEnvironment,
            DesktopEnvironment::Headless => CapabilityState::Unavailable,
        },
        // Nothing else has a Linux implementation behind it yet, so nothing
        // else may be claimed in any session.
        Capability::FileSearch
        | Capability::Clipboard
        | Capability::UriOpen
        | Capability::Notifications
        | Capability::Icons
        | Capability::FileWatching
        | Capability::SecretStorage
        | Capability::ShellIntegration => CapabilityState::Unavailable,
    }
}

/// The capabilities that must never be claimed, whatever the session is.
const UNBACKED: [Capability; 8] = [
    Capability::Clipboard,
    Capability::SecretStorage,
    Capability::Notifications,
    Capability::Icons,
    Capability::FileWatching,
    Capability::FileSearch,
    Capability::UriOpen,
    Capability::ShellIntegration,
];

// ---------------------------------------------------------------------------
// Detecting the session (spec 18.6)
// ---------------------------------------------------------------------------

/// A live Wayland socket names a Wayland session even when DISPLAY is also set.
///
/// Kills the bug where XWayland's compatibility `DISPLAY` is read first and the
/// backend then promises X11 key grabs that the compositor will never deliver.
#[test]
fn a_wayland_socket_wins_over_an_xwayland_display() {
    let environment = detect_desktop_environment(fixture(&[
        ("WAYLAND_DISPLAY", "wayland-0"),
        ("DISPLAY", ":0"),
        ("XDG_SESSION_TYPE", "x11"),
    ]));
    assert_eq!(environment, DesktopEnvironment::Wayland);
}

/// A DISPLAY with no Wayland socket names an X11 session.
#[test]
fn a_display_without_a_wayland_socket_names_an_x11_session() {
    let environment = detect_desktop_environment(fixture(&[("DISPLAY", ":0")]));
    assert_eq!(environment, DesktopEnvironment::X11);
}

/// Neither socket variable set and no session hint means no display at all.
///
/// Kills the bug where a daemon, a container or an SSH session is reported as
/// X11 and every window operation then fails at call time instead of being
/// declared unavailable up front.
#[test]
fn an_environment_with_no_display_variables_at_all_is_headless() {
    let environment = detect_desktop_environment(fixture(&[("HOME", "/home/nobody")]));
    assert_eq!(environment, DesktopEnvironment::Headless);
}

/// An empty display variable is unset, not a display.
///
/// Kills the bug where detection tests only for presence of the key: `DISPLAY=`
/// is exactly what a stripped systemd unit hands a service, and treating it as
/// a display makes the backend claim X11 hotkeys in a headless unit.
#[test]
fn empty_display_variables_count_as_unset_rather_than_as_a_display() {
    let both_empty = detect_desktop_environment(fixture(&[("WAYLAND_DISPLAY", ""), ("DISPLAY", "")]));
    assert_eq!(both_empty, DesktopEnvironment::Headless);

    let empty_wayland_real_x11 =
        detect_desktop_environment(fixture(&[("WAYLAND_DISPLAY", ""), ("DISPLAY", ":0")]));
    assert_eq!(empty_wayland_real_x11, DesktopEnvironment::X11);
}

/// With no socket variable set, XDG_SESSION_TYPE breaks the tie.
#[test]
fn the_session_type_variable_breaks_the_tie_when_no_socket_variable_is_set() {
    let wayland = detect_desktop_environment(fixture(&[("XDG_SESSION_TYPE", "wayland")]));
    assert_eq!(wayland, DesktopEnvironment::Wayland);

    let x11 = detect_desktop_environment(fixture(&[("XDG_SESSION_TYPE", "x11")]));
    assert_eq!(x11, DesktopEnvironment::X11);
}

/// A session type naming no display server leaves the session headless.
///
/// `XDG_SESSION_TYPE=tty` is what logind sets for a console login; it is a
/// positive statement that there is no display, so it must not be read as a
/// vague hint that one might exist.
#[test]
fn a_session_type_that_names_no_display_server_stays_headless() {
    let tty = detect_desktop_environment(fixture(&[("XDG_SESSION_TYPE", "tty")]));
    assert_eq!(tty, DesktopEnvironment::Headless);
}

/// A socket variable outranks a stale session type in both directions.
///
/// Kills the bug where the cheap `XDG_SESSION_TYPE` string is consulted first:
/// it is inherited across `su`, screen sessions and user units and routinely
/// disagrees with the socket that actually exists.
#[test]
fn a_present_socket_outranks_a_disagreeing_session_type() {
    let socket_says_x11 =
        detect_desktop_environment(fixture(&[("DISPLAY", ":0"), ("XDG_SESSION_TYPE", "wayland")]));
    assert_eq!(socket_says_x11, DesktopEnvironment::X11);

    let socket_says_wayland = detect_desktop_environment(fixture(&[
        ("WAYLAND_DISPLAY", "wayland-1"),
        ("XDG_SESSION_TYPE", "tty"),
    ]));
    assert_eq!(socket_says_wayland, DesktopEnvironment::Wayland);
}

// ---------------------------------------------------------------------------
// Reporting for that session (spec 18.2)
// ---------------------------------------------------------------------------

/// X11 has `GrabKey`, so global shortcuts are claimed outright there; window
/// control is claimed only as far as the session can promise it.
///
/// The split is the point. Kills the bug this finding exists for: reporting
/// window control `Available` from the session label alone, when
/// `window_service()` returns `None` on any X display without an EWMH window
/// manager. `Partial` is what "supported by this session, subject to a runtime
/// gate" is called (spec 18.2); hotkeys need no manager and stay `Available`.
#[test]
fn an_x11_session_claims_hotkeys_outright_and_window_control_only_partially() {
    let backend = LinuxBackend::with_desktop_environment(DesktopEnvironment::X11);
    assert_eq!(
        backend.capability(Capability::GlobalHotkeys),
        CapabilityState::Available,
        "X11 GrabKey needs no window manager, so global hotkeys are claimed outright (spec 18.6)"
    );
    for capability in [Capability::WindowEnumeration, Capability::WindowActivation] {
        assert_eq!(
            backend.capability(capability),
            CapabilityState::Partial,
            "{capability:?} under X11 additionally needs an EWMH window manager, which the session \
             label cannot promise (spec 18.2)"
        );
        assert_ne!(
            backend.capability(capability),
            CapabilityState::Available,
            "{capability:?} must not be claimed outright: a bare X server yields no window service"
        );
    }
}

/// Wayland withholds them by protocol, which is a session fact, not a gap in
/// CriKey.
///
/// Kills the bug this whole slice exists for: reporting `Unavailable` under
/// Wayland. `Unavailable` tells a plugin author "CriKey does not do this";
/// `UnsupportedDesktopEnvironment` tells them "this session does not offer it",
/// and only the second is true and actionable (spec 18.2).
#[test]
fn a_wayland_session_reports_hotkeys_and_window_control_as_unsupported_by_the_desktop() {
    let backend = LinuxBackend::with_desktop_environment(DesktopEnvironment::Wayland);
    for capability in [
        Capability::GlobalHotkeys,
        Capability::WindowEnumeration,
        Capability::WindowActivation,
    ] {
        let state = backend.capability(capability);
        assert_eq!(
            state,
            CapabilityState::UnsupportedDesktopEnvironment,
            "{capability:?} is withheld by the compositor, not missing from CriKey (spec 18.2)"
        );
        assert_ne!(
            state,
            CapabilityState::Unavailable,
            "{capability:?} under Wayland must not be flattened onto the generic 'not implemented' answer"
        );
    }
}

/// With no display there is nothing to grab or enumerate at all.
#[test]
fn a_headless_session_reports_hotkeys_and_window_control_as_unavailable() {
    let backend = LinuxBackend::with_desktop_environment(DesktopEnvironment::Headless);
    for capability in [
        Capability::GlobalHotkeys,
        Capability::WindowEnumeration,
        Capability::WindowActivation,
    ] {
        assert_eq!(
            backend.capability(capability),
            CapabilityState::Unavailable,
            "{capability:?} needs a display server that a headless session does not have"
        );
    }
}

/// The three sessions must give genuinely different answers.
///
/// Kills the stub that returns one constant for everything: no single
/// `CapabilityState` can satisfy all three of these at once.
#[test]
fn the_three_sessions_disagree_about_global_hotkeys() {
    let x11 =
        LinuxBackend::with_desktop_environment(DesktopEnvironment::X11).capability(Capability::GlobalHotkeys);
    let wayland = LinuxBackend::with_desktop_environment(DesktopEnvironment::Wayland)
        .capability(Capability::GlobalHotkeys);
    let headless = LinuxBackend::with_desktop_environment(DesktopEnvironment::Headless)
        .capability(Capability::GlobalHotkeys);

    assert_ne!(
        x11, wayland,
        "X11 and Wayland cannot share one global-hotkey answer"
    );
    assert_ne!(
        wayland, headless,
        "a compositor refusal is not the same as no display"
    );
    assert_ne!(
        x11, headless,
        "X11 and headless cannot share one global-hotkey answer"
    );
}

/// Discovery and launching need no display and are claimed in every session.
#[test]
fn discovery_and_launching_stay_available_in_every_session() {
    for environment in ALL_ENVIRONMENTS {
        let label = format!("{environment:?}");
        let backend = LinuxBackend::with_desktop_environment(environment);
        for capability in [Capability::ApplicationDiscovery, Capability::ProcessLaunch] {
            assert_eq!(
                backend.capability(capability),
                CapabilityState::Available,
                "{capability:?} needs no display server and must stay available under {label}"
            );
        }
    }
}

/// Nothing is claimed without an implementation behind it.
///
/// Kills the bug where making the backend session-aware turns into optimism:
/// a session upgrade must not promote clipboard, secrets, notifications, icons,
/// file watching, file search, URI opening or shell integration, none of which
/// have a Linux implementation.
#[test]
fn no_capability_is_claimed_available_without_a_backend_behind_it() {
    for environment in ALL_ENVIRONMENTS {
        let label = format!("{environment:?}");
        let backend = LinuxBackend::with_desktop_environment(environment);
        for capability in UNBACKED {
            assert_ne!(
                backend.capability(capability),
                CapabilityState::Available,
                "{capability:?} has no Linux implementation and must not be claimed under {label} (spec 18.2)"
            );
        }
    }
}

/// Every capability has a deliberately chosen answer in every session.
///
/// [`index_of`] matches exhaustively with no wildcard, so a new `Capability`
/// variant fails to compile here until it is listed; [`required_state`] does
/// the same, so it also fails to compile until somebody decides what Linux
/// reports for it. Together they make "forgot to answer" a build error rather
/// than a silent default.
#[test]
fn every_capability_has_a_deliberate_answer_in_every_session() {
    for (position, capability) in ALL_CAPABILITIES.into_iter().enumerate() {
        assert_eq!(
            index_of(capability),
            position,
            "{capability:?} is listed out of order or twice; the coverage guard is only sound when \
             ALL_CAPABILITIES holds every variant exactly once"
        );
    }

    for environment in ALL_ENVIRONMENTS {
        let label = format!("{environment:?}");
        let backend = LinuxBackend::with_desktop_environment(environment);
        for capability in ALL_CAPABILITIES {
            assert_eq!(
                backend.capability(capability),
                required_state(environment, capability),
                "{capability:?} under {label} does not match the reporting table (spec 18.2)"
            );
        }
    }
}
