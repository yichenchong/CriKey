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
//! Wayland session must be told that window control is not offered by the
//! *session* (`UnsupportedDesktopEnvironment`), not that CriKey lacks it
//! (`Unavailable`), because only the first tells a plugin author that the
//! missing feature is not a CriKey bug and not a permission prompt away.
//!
//! Global shortcuts under Wayland are the one answer that is not a function of
//! the session label at all. The compositor withholds key grabs, and the
//! `GlobalShortcuts` portal grants them back (ADR-0011), so the truthful answer
//! depends on whether a portal is installed -- `Available` when one answers and
//! `Unavailable` when nothing does. These tests inject that probe rather than
//! consulting the build host's session bus, for exactly the reason they inject
//! the environment: a reporting test that passes only on a developer's desktop
//! is not pinning anything.
//!
//! Deliberate non-goals: no test here grabs a key, opens a display or talks to
//! a real portal. This is the reporting surface only; the X11 grab path is
//! pinned by the hotkey tests and the portal path by `wayland_portal.rs`.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;

use crikey_platform::{Capability, CapabilityState};
use crikey_platform_linux::{
    detect_desktop_environment, DesktopEnvironment, FilesystemSearch, LinuxBackend, XdgOpener,
};

/// A backend that reports for `environment`, with the portal probe answered and
/// file search and file opening pinned to explicit services.
///
/// All three injections exist for the same reason: what the backend may claim
/// for global shortcuts depends on an installed portal, what it may claim for
/// file search depends on a readable root and an installed `plocate`, and what
/// it may claim for file opening depends on an installed `xdg-open`. A test
/// that let any of them come from the running session would pass or fail on the
/// build host's packages instead of on the reporting rules.
fn reporting_backend(
    environment: DesktopEnvironment,
    portal: bool,
    indexed: bool,
    opener: bool,
) -> LinuxBackend {
    // A root that exists on every host, because `Available` is a claim about
    // having something to walk. The path is never walked here: reporting must
    // not touch the filesystem beyond deciding that the root is there.
    let roots = vec![std::env::temp_dir()];
    let files = if indexed {
        // Deliberately a path that cannot be spawned: reporting must answer
        // from the *configuration*, not by running the index.
        FilesystemSearch::with_locate(roots, PathBuf::from("/nonexistent/plocate"))
    } else {
        FilesystemSearch::walking(roots)
    };
    // Deliberately unspawnable too, and for the same reason: the claim follows
    // from the session having a helper, not from running it.
    let opener = opener.then(|| XdgOpener::with_helper("/nonexistent/xdg-open"));

    LinuxBackend::with_desktop_environment_and_portal(environment, portal)
        .with_file_search(files)
        .with_file_opener(opener)
}

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
const ALL_CAPABILITIES: [Capability; 15] = [
    Capability::ApplicationDiscovery,
    Capability::FileSearch,
    Capability::Clipboard,
    Capability::GlobalHotkeys,
    Capability::ProcessLaunch,
    Capability::UriOpen,
    Capability::FileOpen,
    Capability::WindowEnumeration,
    Capability::WindowActivation,
    Capability::Notifications,
    Capability::Icons,
    Capability::FileWatching,
    Capability::SecretStorage,
    Capability::ShellIntegration,
    Capability::Compositing,
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
        Capability::FileOpen => 6,
        Capability::WindowEnumeration => 7,
        Capability::WindowActivation => 8,
        Capability::Notifications => 9,
        Capability::Icons => 10,
        Capability::FileWatching => 11,
        Capability::SecretStorage => 12,
        Capability::ShellIntegration => 13,
        Capability::Compositing => 14,
    }
}

/// The state the Linux backend is required to report, capability by capability
/// and session by session. This is the contract table, written out rather than
/// derived, so that an implementation cannot satisfy it by construction.
///
/// `None` for the one entry that has no fixed answer to write down: compositing
/// under X11 is read off the display the suite happens to have inherited, so a
/// row here would pin the build host rather than the backend. That entry is
/// pinned against a server this suite owns, in `compositing_x11.rs`.
fn required_state(
    environment: DesktopEnvironment,
    portal: bool,
    indexed: bool,
    opener: bool,
    capability: Capability,
) -> Option<CapabilityState> {
    let state = match capability {
        // No display server needed: honest everywhere.
        Capability::ApplicationDiscovery | Capability::ProcessLaunch => CapabilityState::Available,
        // Global shortcuts: optional on Linux (spec 18.6). `GrabKey` is core X
        // protocol, so an X11 session delivers them whether or not anything
        // else is running on the display. Under Wayland they exist exactly when
        // the `GlobalShortcuts` portal does, which is a fact about the
        // installation rather than about the compositor.
        Capability::GlobalHotkeys => match environment {
            DesktopEnvironment::X11 => CapabilityState::Available,
            DesktopEnvironment::Wayland if portal => CapabilityState::Available,
            DesktopEnvironment::Wayland => CapabilityState::Unavailable,
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
        // Icons: an XDG theme search plus PNG, SVG, ICO and ICNS decoding, and
        // therefore independent of the session -- a headless unit resolves a
        // themed name exactly as an X11 one does. `Partial` rather than
        // `Available` because there really are theme assets this build finds
        // nothing usable for: `.svgz`, `.xpm`, and the scaled (HiDPI) theme
        // directories it skips.
        Capability::Icons => CapabilityState::Partial,
        // File search: the one capability here that is a function of the
        // filesystem rather than of the session, because a walk needs neither a
        // display nor a daemon. With a readable root it is `Available` in every
        // session including a headless one. An installed `plocate` lowers the
        // claim to `Partial` rather than raising it: that answer comes from an
        // index rebuilt on a timer, so a file saved since the last `updatedb`
        // is missing from it, and a faster mechanism does not license a
        // stronger claim (spec 18.1, 18.2).
        Capability::FileSearch if indexed => CapabilityState::Partial,
        Capability::FileSearch => CapabilityState::Available,
        // Opening a path: a function of whether xdg-utils is installed, which
        // `reporting_backend` injects for the same reason it injects the file
        // search. `Available` with a helper and `Unavailable` without one, and
        // nothing in between: `xdg-open` performs the whole handler lookup
        // itself, so there is no subset of paths a present helper covers.
        Capability::FileOpen if opener => CapabilityState::Available,
        Capability::FileOpen => CapabilityState::Unavailable,
        // The clipboard follows the session and, like window control, without
        // opening a display. X11 selections are core protocol, so an X11 session
        // needs nothing installed for them. Wayland is `Partial` because this
        // backend reaches a Wayland clipboard through XWayland, which every
        // compositor it targets runs and none of them owes it -- the same
        // "supported by this session, subject to a runtime gate" that window
        // control gets under X11. A session with no display server has no
        // clipboard to claim at all.
        Capability::Clipboard => match environment {
            DesktopEnvironment::X11 => CapabilityState::Available,
            DesktopEnvironment::Wayland => CapabilityState::Partial,
            DesktopEnvironment::Headless => CapabilityState::Unavailable,
        },
        // Compositing: `Available` under Wayland with nothing to check, because
        // compositing is what a Wayland compositor *is* -- there is no Wayland
        // session that omits it and still has a display. `Unavailable` with no
        // display server, which composites nothing. Under X11 a compositing
        // manager is a separate, optional program, so the answer is whatever
        // the display says at the moment of asking and no row here can state
        // it; see this function's documentation.
        Capability::Compositing => match environment {
            DesktopEnvironment::X11 => return None,
            DesktopEnvironment::Wayland => CapabilityState::Available,
            DesktopEnvironment::Headless => CapabilityState::Unavailable,
        },
        // Nothing else has a Linux implementation behind it yet, so nothing
        // else may be claimed in any session.
        Capability::UriOpen
        | Capability::Notifications
        | Capability::FileWatching
        | Capability::SecretStorage
        | Capability::ShellIntegration => CapabilityState::Unavailable,
    };

    Some(state)
}

/// The capabilities that must never be claimed, whatever the session is.
///
/// Icons and file search are deliberately absent: both have an implementation
/// behind them, and both are the capabilities here that do not depend on a
/// display server at all. The clipboard is absent too, and for the opposite
/// reason: it has an implementation *and* it depends on the session, so what it
/// may claim is pinned per session in [`required_state`] instead.
const UNBACKED: [Capability; 5] = [
    Capability::SecretStorage,
    Capability::Notifications,
    Capability::FileWatching,
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

/// Wayland reports what the portal can actually do, and keeps window control
/// a session fact rather than a CriKey gap.
///
/// Kills two bugs at once. Reporting `UnsupportedDesktopEnvironment` for
/// global shortcuts on a session that has a portal sends a plugin author
/// looking for a compositor limitation that is not there, and reporting
/// `Available` on a session with no portal is a claim nothing can honour. Both
/// answers come from the same session label, so only the probe can separate
/// them (spec 18.2, ADR-0011).
#[test]
fn a_wayland_session_claims_hotkeys_only_when_a_portal_answers() {
    let with_portal = LinuxBackend::with_desktop_environment_and_portal(DesktopEnvironment::Wayland, true);
    assert_eq!(
        with_portal.capability(Capability::GlobalHotkeys),
        CapabilityState::Available,
        "a Wayland session with a GlobalShortcuts portal really does offer global hotkeys"
    );

    let without_portal =
        LinuxBackend::with_desktop_environment_and_portal(DesktopEnvironment::Wayland, false);
    assert_eq!(
        without_portal.capability(Capability::GlobalHotkeys),
        CapabilityState::Unavailable,
        "with no portal there is nothing behind the claim, and the session type cannot supply one"
    );
}

/// Window control under Wayland is withheld by the session, not missing from
/// CriKey.
///
/// Kills the bug where the portal work is taken as licence to claim the rest:
/// no Wayland protocol lets an ordinary client enumerate another client's
/// windows, and `UnsupportedDesktopEnvironment` says that while `Unavailable`
/// would blame CriKey for it (spec 18.2).
#[test]
fn a_wayland_session_reports_window_control_as_unsupported_by_the_desktop() {
    for portal in [true, false] {
        let backend = LinuxBackend::with_desktop_environment_and_portal(DesktopEnvironment::Wayland, portal);
        for capability in [Capability::WindowEnumeration, Capability::WindowActivation] {
            let state = backend.capability(capability);
            assert_eq!(
                state,
                CapabilityState::UnsupportedDesktopEnvironment,
                "{capability:?} is withheld by the compositor, not missing from CriKey (spec 18.2)"
            );
            assert_ne!(
                state,
                CapabilityState::Unavailable,
                "{capability:?} under Wayland must not be flattened onto the generic 'not \
                 implemented' answer, whatever the portal offers"
            );
        }
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

/// The sessions must give genuinely different answers.
///
/// Kills the stub that returns one constant for everything: no single
/// `CapabilityState` satisfies window control across the three sessions, and
/// no session-only answer satisfies global hotkeys across the two Wayland
/// installations -- same compositor, opposite truths.
#[test]
fn the_sessions_disagree_about_what_they_can_carry() {
    let x11 = LinuxBackend::with_desktop_environment(DesktopEnvironment::X11);
    let wayland = LinuxBackend::with_desktop_environment_and_portal(DesktopEnvironment::Wayland, true);
    let headless = LinuxBackend::with_desktop_environment(DesktopEnvironment::Headless);

    for (left, right, reason) in [
        (
            &x11,
            &wayland,
            "X11 and Wayland cannot share one window-control answer",
        ),
        (
            &wayland,
            &headless,
            "a compositor refusal is not the same as no display",
        ),
        (
            &x11,
            &headless,
            "X11 and headless cannot share one window-control answer",
        ),
    ] {
        assert_ne!(
            left.capability(Capability::WindowEnumeration),
            right.capability(Capability::WindowEnumeration),
            "{reason}"
        );
    }

    let portalless = LinuxBackend::with_desktop_environment_and_portal(DesktopEnvironment::Wayland, false);
    assert_ne!(
        wayland.capability(Capability::GlobalHotkeys),
        portalless.capability(Capability::GlobalHotkeys),
        "the same session type with and without a portal cannot share one global-hotkey answer"
    );
    assert_ne!(
        x11.capability(Capability::GlobalHotkeys),
        headless.capability(Capability::GlobalHotkeys),
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

/// The clipboard is claimed against the session it is reached through.
///
/// Kills two bugs in opposite directions. Claiming `Available` for a headless
/// unit hands the user a copy action whose only outcome is a failure at the
/// moment they select it -- the clipboard implementation is an X11 client and a
/// headless session has no server to be a client of. Reporting `Unavailable`
/// under Wayland would be the other lie: the clipboard is reached there, through
/// XWayland, and only the absence of XWayland can stop it, which is a runtime
/// gate and therefore `Partial` (spec 18.2).
///
/// That the *service* agrees with this report is pinned in `clipboard_x11.rs`,
/// where a reachable display exists to make the assertion mean something.
#[test]
fn the_clipboard_is_claimed_against_the_session_it_is_reached_through() {
    for (environment, expected) in [
        (DesktopEnvironment::X11, CapabilityState::Available),
        (DesktopEnvironment::Wayland, CapabilityState::Partial),
        (DesktopEnvironment::Headless, CapabilityState::Unavailable),
    ] {
        let backend = LinuxBackend::with_desktop_environment(environment);
        assert_eq!(
            backend.capability(Capability::Clipboard),
            expected,
            "{environment:?} must report the clipboard it can actually reach"
        );
    }
}

/// Nothing is claimed without an implementation behind it.
///
/// Kills the bug where making the backend session-aware turns into optimism:
/// a session upgrade must not promote secrets, notifications, file watching,
/// URI opening or shell integration, none of which have a Linux implementation.
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
        for portal in [true, false] {
            for indexed in [true, false] {
                for opener in [true, false] {
                    let label = format!("{environment:?}");
                    let backend = reporting_backend(environment, portal, indexed, opener);
                    for capability in ALL_CAPABILITIES {
                        // `None` is the table declining to state an answer that
                        // depends on the display this suite inherited rather
                        // than on the reporting rules; `compositing_x11.rs`
                        // owns that one.
                        let Some(required) = required_state(environment, portal, indexed, opener, capability)
                        else {
                            continue;
                        };
                        assert_eq!(
                            backend.capability(capability),
                            required,
                            "{capability:?} under {label} with portal={portal} indexed={indexed} \
                             opener={opener} does not match the reporting table (spec 18.2)"
                        );
                    }
                }
            }
        }
    }
}

/// File search is claimed in every session, and an index lowers the claim.
///
/// Kills two bugs. Reporting file search from the session label -- the shape
/// every other optional capability here has -- would make a headless unit claim
/// nothing, when a walk of `$HOME` needs no display at all. And reporting
/// `Available` while delegating to `plocate` promises live results from an index
/// that `updatedb` refreshes on a timer; the file the user saved a minute ago is
/// exactly the one missing from it (spec 18.1, 18.2).
#[test]
fn file_search_is_claimed_in_every_session_and_an_index_only_lowers_the_claim() {
    for environment in ALL_ENVIRONMENTS {
        let label = format!("{environment:?}");

        let walking = reporting_backend(environment, false, false, true);
        assert_eq!(
            walking.capability(Capability::FileSearch),
            CapabilityState::Available,
            "a readable root is all a walk needs, so file search is claimed under {label}"
        );

        let indexed = reporting_backend(environment, false, true, true);
        assert_eq!(
            indexed.capability(Capability::FileSearch),
            CapabilityState::Partial,
            "an answer from a periodically rebuilt index does not cover everything under {label}"
        );
    }
}

/// A session with nothing to search claims nothing and hands out no service.
///
/// Kills the bug where the walk's "it always works" reasoning is taken too far:
/// a systemd unit or container with no `$HOME` gives the walk no root, and a
/// service that can only ever return empty answers teaches the user the feature
/// is broken rather than that it has nowhere to look (spec 18.2).
#[test]
fn a_session_with_no_readable_root_claims_no_file_search_at_all() {
    let backend = LinuxBackend::with_desktop_environment(DesktopEnvironment::X11)
        .with_file_search(FilesystemSearch::walking(Vec::new()));

    assert_eq!(
        backend.capability(Capability::FileSearch),
        CapabilityState::Unavailable,
        "with no root there is nothing behind the claim"
    );
    assert!(
        backend.file_search().is_none(),
        "a service with no root must not be handed out: it can only answer empty forever"
    );
}

/// The service really is handed out when there is a root, and names its
/// mechanism.
#[test]
fn a_session_with_a_root_hands_out_a_file_search_service_that_names_its_mechanism() {
    let backend = LinuxBackend::with_desktop_environment(DesktopEnvironment::Headless)
        .with_file_search(FilesystemSearch::walking(vec![std::env::temp_dir()]));

    let service = backend
        .file_search()
        .expect("a backend with a readable root offers file search");
    assert_eq!(
        service.source_name(),
        "filesystem-walk",
        "the diagnostic names the mechanism that answered, not the platform"
    );
}
