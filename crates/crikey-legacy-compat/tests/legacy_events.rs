//! Legacy event semantics and activation/deactivation coalescing (spec 14.6,
//! 14.7, 13.3-13.4, 18.7, 26.2; roadmap M3; acceptance 31.28).
//!
//! This suite defends the contract of `EventCoalescer`, the host-side component
//! that turns raw operating-system notifications into the *logical* legacy
//! events a Keypirinha plugin observes through `on_events(flags)`, and that
//! reproduces the documented activation/deactivation coalescing.
//!
//! The whole point of the type is the asymmetry spec 18.7 states in one
//! sentence: "for legacy plugins, low-level event noise may be coalesced before
//! translation, but semantic legacy events shall not receive arbitrary
//! additional debounce delays". So the coalescer has exactly one time-driven
//! stage — the raw filesystem window in front of translation — and everything
//! downstream of translation is immediate. A coalescer that simply reused the
//! modern `Debouncer` on semantic events would pass none of the timing
//! assertions below.
//!
//! Model pinned by these tests:
//!
//! * Time is virtual. Every timestamp is an explicit `Millis` argument; no test
//!   sleeps, reads a wall clock, or spawns a thread.
//! * Inputs are recorded (`post_event`, `broadcast_event`,
//!   `note_raw_filesystem`, `note_activated`, `note_deactivated`) and have no
//!   delivery side effect. `tick(now)` performs every delivery that is due at
//!   or before `now`, so a test asserting on what happens "at" `t` records at
//!   `t` and then calls `tick(t)`.
//! * A tick returns at most one `EventDelivery` per plugin instance, because a
//!   returned delivery *is* an in-flight callback (spec 13.4). The host reports
//!   its end with `end_callback`, and the next tick may then deliver the next
//!   pending notice for that instance.
//! * `next_wakeup()` reports only *time-driven* deadlines: the raw filesystem
//!   window / maximum wait, and the activation coalescing window. Work held
//!   back purely by callback serialization is not a deadline — it becomes
//!   deliverable the moment `end_callback` is called, so the host ticks after
//!   every callback end.
//! * `LegacyEventFlags` mirrors the documented `keypirinha.Events` flag set
//!   bit-for-bit so the Python shim can expose the same integers:
//!   `APP_CONFIG`/`APPCONFIG`, `PACKAGE_CONFIG`/`PACKCONFIG`,
//!   `NETWORK_OPTIONS`/`NETOPTIONS`, `DESKTOP`, `START_MENU`/`STARTMENU`, plus
//!   the two CriKey extensions `PACKAGES` (the installed package set changed)
//!   and `FILESYSTEM` (a watched path changed).
//! * Diagnostic counters (spec 26.2) are part of the contract: raw-noise
//!   reduction is only observable through them, and acceptance 31.28 requires
//!   asserting *both* that the noise was reduced and that the semantic
//!   behaviour was unchanged.

use std::path::PathBuf;

use crikey_core::PluginId;
use crikey_input_scheduler::{Millis, SchedulingProfile};
use crikey_legacy_compat::{
    ActivationState, CallbackOutcome, CoalescerConfig, EventCoalescer, EventDelivery, LegacyCallback,
    LegacyEventFlags, RawFilesystemKind, RawFilesystemNotification, WatchScope,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_string())
}

fn config_with(
    raw_filesystem_window_ms: Millis,
    raw_filesystem_maximum_wait_ms: Millis,
    activation_window_ms: Millis,
) -> CoalescerConfig {
    CoalescerConfig {
        raw_filesystem_window_ms,
        raw_filesystem_maximum_wait_ms,
        activation_window_ms,
    }
}

/// A coalescer configured with a deliberately *long* raw filesystem window and
/// a long activation window. Semantic-event tests use this configuration on
/// purpose: if any of those windows ever leaked onto a semantic legacy event,
/// the timing assertions would fail loudly instead of passing by accident.
fn config() -> CoalescerConfig {
    config_with(20, 200, 50)
}

fn coalescer_with(config: CoalescerConfig, plugins: &[&PluginId]) -> EventCoalescer {
    let mut coalescer = EventCoalescer::new(config);
    for plugin in plugins {
        coalescer.register_plugin((*plugin).clone());
    }
    coalescer
}

fn coalescer(plugins: &[&PluginId]) -> EventCoalescer {
    coalescer_with(config(), plugins)
}

fn raw(scope: WatchScope, path: &str, kind: RawFilesystemKind) -> RawFilesystemNotification {
    RawFilesystemNotification {
        scope,
        path: PathBuf::from(path),
        kind,
    }
}

// ---------------------------------------------------------------------------
// Projections
//
// Deliveries are compared as plain tuples so a failure prints the whole
// observed delivery schedule rather than a struct-by-struct diff. At most one
// delivery per plugin can occur in a single tick, so sorting by plugin id
// inside a tick is lossless and keeps multi-plugin assertions independent of
// the fan-out order.
// ---------------------------------------------------------------------------

fn shape(deliveries: &[EventDelivery]) -> Vec<(&str, LegacyCallback, LegacyEventFlags, Millis)> {
    let mut shaped: Vec<_> = deliveries
        .iter()
        .map(|delivery| {
            (
                delivery.plugin.0.as_str(),
                delivery.callback,
                delivery.flags,
                delivery.at,
            )
        })
        .collect();
    shaped.sort_by(|left, right| left.0.cmp(right.0));
    shaped
}

fn callbacks(log: &[EventDelivery]) -> Vec<(LegacyCallback, Millis)> {
    log.iter()
        .map(|delivery| (delivery.callback, delivery.at))
        .collect()
}

// ---------------------------------------------------------------------------
// The flag set itself
// ---------------------------------------------------------------------------

#[test]
fn the_legacy_event_flag_set_mirrors_the_documented_event_kinds() {
    // Spec 14.4 ("Events") + 14.2: the compatibility layer reproduces the
    // documented `keypirinha.Events` flags, so the bit values are part of the
    // ABI seen by unchanged plugin source through the Python shim
    // (`python/keypirinha.py` exposes exactly these integers as an
    // `enum.IntFlag`). PACKAGES and FILESYSTEM are CriKey extensions for the
    // package set (spec 14.3) and for watched paths that have no documented
    // legacy flag of their own (spec 18.7).
    assert_eq!(
        [
            LegacyEventFlags::APP_CONFIG.bits(),
            LegacyEventFlags::PACKAGE_CONFIG.bits(),
            LegacyEventFlags::NETWORK_OPTIONS.bits(),
            LegacyEventFlags::PACKAGES.bits(),
            LegacyEventFlags::FILESYSTEM.bits(),
            LegacyEventFlags::DESKTOP.bits(),
            LegacyEventFlags::START_MENU.bits(),
        ],
        [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40],
        "spec 14.2/14.4: legacy event flag bits are an ABI shared with the Python shim"
    );
    assert_eq!(
        LegacyEventFlags::ALL.bits(),
        0x7f,
        "spec 14.4: ALL must be exactly the union of the defined flags"
    );

    assert!(
        LegacyEventFlags::empty().is_empty(),
        "spec 14.6: the empty flag set is the identity of the union"
    );
    assert!(
        !LegacyEventFlags::APP_CONFIG.is_empty(),
        "spec 14.6: a named flag is never empty"
    );

    let both = LegacyEventFlags::APP_CONFIG | LegacyEventFlags::PACKAGE_CONFIG;
    assert!(
        both.contains(LegacyEventFlags::APP_CONFIG) && both.contains(LegacyEventFlags::PACKAGE_CONFIG),
        "spec 14.6: a union contains each of its members"
    );
    assert!(
        !both.contains(LegacyEventFlags::NETWORK_OPTIONS),
        "spec 14.6: a union must not contain flags nobody raised"
    );
    assert!(
        both.intersects(LegacyEventFlags::APP_CONFIG | LegacyEventFlags::DESKTOP),
        "spec 14.6: intersection tests the overlapping subset"
    );
    assert_eq!(
        both | LegacyEventFlags::APP_CONFIG,
        both,
        "spec 14.6: combining an already-pending flag is idempotent"
    );

    let mut accumulating = LegacyEventFlags::empty();
    accumulating |= LegacyEventFlags::NETWORK_OPTIONS;
    accumulating.insert(LegacyEventFlags::DESKTOP);
    assert_eq!(
        accumulating,
        LegacyEventFlags::NETWORK_OPTIONS | LegacyEventFlags::DESKTOP,
        "spec 14.6: flags accumulate into the pending set"
    );
    accumulating.remove(LegacyEventFlags::DESKTOP);
    assert_eq!(
        accumulating,
        LegacyEventFlags::NETWORK_OPTIONS,
        "spec 14.6: removal clears only the named flag"
    );
    assert_eq!(
        accumulating & LegacyEventFlags::ALL,
        accumulating,
        "spec 14.4: every named flag is a member of ALL"
    );

    assert_eq!(
        LegacyEventFlags::from_bits(0x03),
        Some(LegacyEventFlags::APP_CONFIG | LegacyEventFlags::PACKAGE_CONFIG),
        "spec 14.2: the wire form of a flag set is its bits"
    );
    assert_eq!(
        LegacyEventFlags::from_bits(0x80),
        None,
        "spec 14.2: an undefined bit must be rejected, never silently kept"
    );

    // Translation of a watched filesystem scope into legacy flags (spec 18.7).
    assert_eq!(
        LegacyEventFlags::for_watch_scope(WatchScope::ApplicationConfig),
        LegacyEventFlags::APP_CONFIG | LegacyEventFlags::FILESYSTEM,
        "spec 18.7: application config changes translate to APP_CONFIG"
    );
    assert_eq!(
        LegacyEventFlags::for_watch_scope(WatchScope::PackageConfig),
        LegacyEventFlags::PACKAGE_CONFIG | LegacyEventFlags::FILESYSTEM,
        "spec 18.7: package config changes translate to PACKAGE_CONFIG"
    );
    assert_eq!(
        LegacyEventFlags::for_watch_scope(WatchScope::PackageFiles),
        LegacyEventFlags::PACKAGES | LegacyEventFlags::FILESYSTEM,
        "spec 14.3/18.7: package installs and removals translate to PACKAGES"
    );
    assert_eq!(
        LegacyEventFlags::for_watch_scope(WatchScope::Desktop),
        LegacyEventFlags::DESKTOP | LegacyEventFlags::FILESYSTEM,
        "spec 18.7: the desktop watcher translates to DESKTOP"
    );
    assert_eq!(
        LegacyEventFlags::for_watch_scope(WatchScope::StartMenu),
        LegacyEventFlags::START_MENU | LegacyEventFlags::FILESYSTEM,
        "spec 18.7: the start menu watcher translates to START_MENU"
    );
    assert_eq!(
        LegacyEventFlags::for_watch_scope(WatchScope::PluginData),
        LegacyEventFlags::FILESYSTEM,
        "spec 18.7: a plain watched path carries only FILESYSTEM"
    );
}

// ---------------------------------------------------------------------------
// Flag union (spec 14.6)
// ---------------------------------------------------------------------------

#[test]
fn event_flags_pending_for_the_same_delivery_are_combined_into_one_callback() {
    // Spec 14.6: "it may combine event flags already pending for immediate
    // delivery". Three notices recorded before the tick must produce exactly
    // one `on_events` callback carrying their union, not three callbacks and
    // not three flag sets.
    let plugin = plugin("legacy.everything");
    let mut coalescer = coalescer(&[&plugin]);

    coalescer.post_event(&plugin, LegacyEventFlags::APP_CONFIG, 40);
    coalescer.post_event(&plugin, LegacyEventFlags::PACKAGE_CONFIG, 40);
    coalescer.post_event(
        &plugin,
        LegacyEventFlags::APP_CONFIG | LegacyEventFlags::NETWORK_OPTIONS,
        40,
    );

    let union =
        LegacyEventFlags::APP_CONFIG | LegacyEventFlags::PACKAGE_CONFIG | LegacyEventFlags::NETWORK_OPTIONS;
    assert_eq!(
        coalescer.pending_flags(&plugin),
        union,
        "spec 14.6: pending flags accumulate into a single set per instance"
    );

    let delivered = coalescer.tick(40);
    assert_eq!(
        shape(&delivered),
        vec![("legacy.everything", LegacyCallback::OnEvents, union, 40)],
        "spec 14.6: flags pending for the same delivery are combined into ONE on_events call"
    );
    assert_eq!(
        coalescer.pending_flags(&plugin),
        LegacyEventFlags::empty(),
        "spec 14.6: a delivered union leaves nothing pending"
    );

    coalescer.end_callback(&plugin, CallbackOutcome::Completed, 41);
    assert!(
        coalescer.tick(41).is_empty(),
        "spec 14.6: a combined delivery must not be repeated once per merged flag"
    );

    let diagnostics = coalescer.diagnostics();
    assert_eq!(
        diagnostics.events_delivered, 1,
        "spec 14.6: three notices, one callback"
    );
    assert_eq!(
        diagnostics.flag_unions_merged, 2,
        "spec 26.2: the two later notices merged into the already-pending delivery"
    );
}

// ---------------------------------------------------------------------------
// No debounce on semantic events (spec 14.6, 18.7)
// ---------------------------------------------------------------------------

#[test]
fn a_semantic_legacy_event_is_delivered_at_its_own_timestamp_without_a_debounce_window() {
    // Spec 14.6: "it shall not add arbitrary time-based debounce windows to
    // semantic legacy events", and 18.7 repeats the rule for translated
    // filesystem events. This is the sharp contrast with the modern side: spec
    // 18.8 coalesces rapid *modern* configuration changes behind a quiet
    // period, and modern query dispatch is debounced per policy. Legacy strict
    // has no such window anywhere.
    assert!(
        !SchedulingProfile::LegacyStrict.allows_time_debounce(),
        "spec 8.4/14.5/31.14: legacy-strict is never time debounced"
    );

    let plugin = plugin("legacy.prompt");
    // The configuration deliberately carries a 20 ms raw filesystem window and
    // a 50 ms activation window. Neither may touch a semantic event: the events
    // at 7 ms and 9 ms fall inside both windows and must still be delivered
    // separately at their own timestamps.
    let mut coalescer = coalescer(&[&plugin]);

    for at in [0u64, 7, 9, 1_000] {
        coalescer.post_event(&plugin, LegacyEventFlags::PACKAGE_CONFIG, at);
        assert_eq!(
            coalescer.next_wakeup(),
            None,
            "spec 14.6: a semantic legacy event must not arm a debounce deadline (at {at} ms)"
        );

        assert_eq!(
            shape(&coalescer.tick(at)),
            vec![(
                "legacy.prompt",
                LegacyCallback::OnEvents,
                LegacyEventFlags::PACKAGE_CONFIG,
                at
            )],
            "spec 14.6/18.7: a single logical event is delivered promptly at its own timestamp"
        );
        coalescer.end_callback(&plugin, CallbackOutcome::Completed, at);
    }

    let diagnostics = coalescer.diagnostics();
    assert_eq!(
        diagnostics.events_delivered, 4,
        "spec 14.6: four logical events, four callbacks - none was deferred into another"
    );
    assert_eq!(
        diagnostics.flag_unions_merged, 0,
        "spec 14.6: nothing can merge when every event is delivered immediately"
    );
    assert_eq!(
        coalescer.next_wakeup(),
        None,
        "spec 14.6: no residual deadline may survive a semantic delivery"
    );
}

// ---------------------------------------------------------------------------
// Raw notification collapsing (spec 14.6, 18.7, acceptance 31.28)
// ---------------------------------------------------------------------------

#[test]
fn a_raw_filesystem_burst_collapses_into_exactly_one_semantic_legacy_event() {
    // Acceptance 31.28: "raw filesystem notification bursts are coalesced
    // without changing semantic legacy event behavior". Both halves are
    // asserted here: the raw count drops (path- and event-type coalescing,
    // spec 18.7) and the semantic event still arrives exactly once, once per
    // plugin, carrying the union of the translated scopes.
    let first = plugin("legacy.watcher.a");
    let second = plugin("legacy.watcher.b");
    let mut coalescer = coalescer(&[&first, &second]);

    let burst = [
        (
            0u64,
            WatchScope::Desktop,
            "/home/dev/Desktop/notes.desktop",
            RawFilesystemKind::Created,
        ),
        (
            2,
            WatchScope::Desktop,
            "/home/dev/Desktop/notes.desktop",
            RawFilesystemKind::Modified,
        ),
        (
            4,
            WatchScope::Desktop,
            "/home/dev/Desktop/notes.desktop",
            RawFilesystemKind::Modified,
        ),
        (
            6,
            WatchScope::Desktop,
            "/home/dev/Desktop/.tmp-notes",
            RawFilesystemKind::Removed,
        ),
        (
            8,
            WatchScope::PackageConfig,
            "/home/dev/.config/crikey/packages/Foo/foo.ini",
            RawFilesystemKind::Modified,
        ),
        (
            10,
            WatchScope::PackageConfig,
            "/home/dev/.config/crikey/packages/Foo/foo.ini",
            RawFilesystemKind::Renamed,
        ),
    ];
    for (at, scope, path, kind) in burst {
        coalescer.note_raw_filesystem(raw(scope, path, kind), at);
        assert!(
            coalescer.tick(at).is_empty(),
            "spec 18.7: raw notifications are coalesced BEFORE translation (at {at} ms)"
        );
    }

    assert_eq!(
        coalescer.next_wakeup(),
        Some(30),
        "spec 18.7: the raw window is measured from the last raw notification"
    );
    assert!(
        coalescer.tick(29).is_empty(),
        "spec 18.7: the burst is still open one millisecond before the window closes"
    );

    let translated =
        LegacyEventFlags::DESKTOP | LegacyEventFlags::PACKAGE_CONFIG | LegacyEventFlags::FILESYSTEM;
    assert_eq!(
        shape(&coalescer.tick(30)),
        vec![
            ("legacy.watcher.a", LegacyCallback::OnEvents, translated, 30),
            ("legacy.watcher.b", LegacyCallback::OnEvents, translated, 30),
        ],
        "spec 14.6/31.28: the burst becomes ONE logical legacy event per plugin, carrying the \
         union of the translated watch scopes"
    );

    let diagnostics = coalescer.diagnostics();
    assert_eq!(
        diagnostics.raw_notifications_seen, 6,
        "spec 26.2: every raw notification is accounted for"
    );
    assert_eq!(
        diagnostics.raw_notifications_collapsed, 5,
        "acceptance 31.28: raw noise must actually be reduced, here six raw events to one"
    );
    assert_eq!(
        diagnostics.logical_events_translated, 1,
        "spec 18.7: exactly one logical event crosses the translation boundary"
    );
    assert_eq!(
        diagnostics.events_delivered, 2,
        "spec 14.6: one semantic delivery per registered plugin, no duplicates"
    );

    coalescer.end_callback(&first, CallbackOutcome::Completed, 31);
    coalescer.end_callback(&second, CallbackOutcome::Completed, 31);
    assert!(
        coalescer.tick(31).is_empty(),
        "acceptance 31.28: collapsing must not smuggle a second semantic delivery"
    );
    assert_eq!(
        coalescer.next_wakeup(),
        None,
        "spec 18.7: a flushed burst leaves no deadline armed"
    );

    // A sustained burst must still resolve: spec 18.7 requires maximum-wait
    // flushes so a stream of raw events cannot postpone translation forever.
    let sustained = plugin("legacy.watcher.sustained");
    let mut coalescer = coalescer_with(config_with(20, 60, 50), &[&sustained]);
    for at in [0u64, 10, 20, 30, 40, 50] {
        coalescer.note_raw_filesystem(
            raw(
                WatchScope::StartMenu,
                "/home/dev/.local/share/applications/foo.desktop",
                RawFilesystemKind::Modified,
            ),
            at,
        );
        assert!(
            coalescer.tick(at).is_empty(),
            "spec 18.7: a sustained burst stays open while notifications keep arriving \
             (at {at} ms)"
        );
    }
    assert_eq!(
        coalescer.next_wakeup(),
        Some(60),
        "spec 18.7: the maximum wait bounds a sustained burst, overriding the sliding window"
    );
    assert!(
        coalescer.tick(59).is_empty(),
        "spec 18.7: the maximum wait has not elapsed yet"
    );
    assert_eq!(
        shape(&coalescer.tick(60)),
        vec![(
            "legacy.watcher.sustained",
            LegacyCallback::OnEvents,
            LegacyEventFlags::START_MENU | LegacyEventFlags::FILESYSTEM,
            60
        )],
        "spec 18.7/31.28: a maximum-wait flush still yields exactly one semantic event"
    );
    assert_eq!(
        coalescer.diagnostics().raw_notifications_collapsed,
        5,
        "acceptance 31.28: six sustained raw notifications collapse to one logical event"
    );
}

// ---------------------------------------------------------------------------
// Activation and deactivation coalescing (spec 14.7)
// ---------------------------------------------------------------------------

#[test]
fn a_later_activation_supersedes_a_pending_deactivation() {
    // Spec 14.7: "a later activation may supersede a pending deactivation".
    // The plugin must observe the net state - it stays activated - and must
    // never receive the bogus `on_deactivated` callback for the flap.
    let plugin = plugin("legacy.flapping");
    let mut coalescer = coalescer(&[&plugin]);
    let mut log: Vec<EventDelivery> = Vec::new();

    coalescer.note_activated(&plugin, 0);
    let activated = coalescer.tick(0);
    assert_eq!(
        shape(&activated),
        vec![(
            "legacy.flapping",
            LegacyCallback::OnActivated,
            LegacyEventFlags::empty(),
            0
        )],
        "spec 14.7: an activation is delivered promptly, it is not held"
    );
    log.extend(activated);
    coalescer.end_callback(&plugin, CallbackOutcome::Completed, 1);
    assert_eq!(
        coalescer.activation_state(&plugin),
        ActivationState::Activated,
        "spec 14.7: the delivered activation is the current net state"
    );

    coalescer.note_deactivated(&plugin, 10);
    assert!(
        coalescer.tick(10).is_empty(),
        "spec 14.7: a deactivation is held for the coalescing window so it can be superseded"
    );
    assert_eq!(
        coalescer.next_wakeup(),
        Some(60),
        "spec 14.7: the pending deactivation is the only armed deadline"
    );

    coalescer.note_activated(&plugin, 15);
    assert_eq!(
        coalescer.next_wakeup(),
        None,
        "spec 14.7: the superseded deactivation must not stay armed"
    );
    let reactivated = coalescer.tick(15);
    assert_eq!(
        shape(&reactivated),
        vec![(
            "legacy.flapping",
            LegacyCallback::OnActivated,
            LegacyEventFlags::empty(),
            15
        )],
        "spec 14.7: the later activation itself is still delivered"
    );
    log.extend(reactivated);
    coalescer.end_callback(&plugin, CallbackOutcome::Completed, 16);

    log.extend(coalescer.tick(60));
    log.extend(coalescer.tick(1_000));
    assert_eq!(
        callbacks(&log),
        vec![
            (LegacyCallback::OnActivated, 0),
            (LegacyCallback::OnActivated, 15),
        ],
        "spec 14.7: no bogus on_deactivated may be delivered for a superseded deactivation"
    );
    assert_eq!(
        coalescer.activation_state(&plugin),
        ActivationState::Activated,
        "spec 14.7: the plugin sees the net state, which is activated"
    );

    let diagnostics = coalescer.diagnostics();
    assert_eq!(
        diagnostics.superseded_deactivations, 1,
        "spec 26.2: the superseded deactivation is diagnosable, not invisible"
    );
    assert_eq!(
        diagnostics.synthesized_callbacks, 0,
        "spec 14.7: the layer never invents a callback the host did not observe"
    );
}

#[test]
fn two_activations_in_a_row_are_delivered_without_synthesizing_alternation() {
    // Spec 14.7: "legacy plugins shall not be promised strict alternation
    // between activation and deactivation callbacks". Repeats in either
    // direction are legal and are passed through: the layer must neither
    // suppress the second one nor manufacture the missing opposite callback.
    let plugin = plugin("legacy.no.alternation");
    let mut coalescer = coalescer(&[&plugin]);
    let mut log: Vec<EventDelivery> = Vec::new();

    for at in [0u64, 30] {
        coalescer.note_activated(&plugin, at);
        let delivered = coalescer.tick(at);
        assert_eq!(
            shape(&delivered),
            vec![(
                "legacy.no.alternation",
                LegacyCallback::OnActivated,
                LegacyEventFlags::empty(),
                at
            )],
            "spec 14.7: an activation without an intervening deactivation is still delivered \
             (at {at} ms)"
        );
        log.extend(delivered);
        coalescer.end_callback(&plugin, CallbackOutcome::Completed, at + 1);
    }
    assert_eq!(
        coalescer.activation_state(&plugin),
        ActivationState::Activated,
        "spec 14.7: a repeated activation leaves the instance activated"
    );

    // The same non-alternation rule holds in the other direction: two
    // deactivations, each flushed after its own coalescing window.
    for at in [100u64, 200] {
        coalescer.note_deactivated(&plugin, at);
        assert!(
            coalescer.tick(at).is_empty(),
            "spec 14.7: a deactivation waits out the coalescing window (at {at} ms)"
        );
        let delivered = coalescer.tick(at + 50);
        assert_eq!(
            shape(&delivered),
            vec![(
                "legacy.no.alternation",
                LegacyCallback::OnDeactivated,
                LegacyEventFlags::empty(),
                at + 50
            )],
            "spec 14.7: an unsuperseded deactivation is delivered when its window closes"
        );
        log.extend(delivered);
        coalescer.end_callback(&plugin, CallbackOutcome::Completed, at + 51);
    }

    assert_eq!(
        callbacks(&log),
        vec![
            (LegacyCallback::OnActivated, 0),
            (LegacyCallback::OnActivated, 30),
            (LegacyCallback::OnDeactivated, 150),
            (LegacyCallback::OnDeactivated, 250),
        ],
        "spec 14.7: legacy plugins are not promised strict alternation, so repeats pass through \
         unsuppressed and unpadded"
    );
    assert_eq!(
        coalescer.diagnostics().synthesized_callbacks,
        0,
        "spec 14.7: no deactivation may be synthesized between two activations, and no \
         activation between two deactivations"
    );
    assert_eq!(
        coalescer.diagnostics().superseded_deactivations,
        0,
        "spec 14.7: nothing was superseded in this timeline"
    );
    assert_eq!(
        coalescer.activation_state(&plugin),
        ActivationState::Deactivated,
        "spec 14.7: a repeated deactivation leaves the instance deactivated"
    );
}

// ---------------------------------------------------------------------------
// Serialization with the plugin's other callbacks (spec 13.3, 13.4, 14.5)
// ---------------------------------------------------------------------------

#[test]
fn an_event_never_interleaves_with_an_in_flight_callback_for_the_same_instance() {
    // Spec 13.4 + 14.5 + 31.16: no two lifecycle callbacks run concurrently
    // against the same legacy plugin instance, and `on_events` is one of them
    // (spec 13.2). Spec 13.3 keeps the constraint per instance: a different
    // plugin must not be blocked by it.
    let busy = plugin("legacy.busy");
    let idle = plugin("legacy.idle");
    let mut coalescer = coalescer(&[&busy, &idle]);

    coalescer.begin_callback(&busy, LegacyCallback::OnSuggest, 10);

    coalescer.post_event(&busy, LegacyEventFlags::APP_CONFIG, 12);
    coalescer.post_event(&idle, LegacyEventFlags::NETWORK_OPTIONS, 12);
    assert_eq!(
        shape(&coalescer.tick(12)),
        vec![(
            "legacy.idle",
            LegacyCallback::OnEvents,
            LegacyEventFlags::NETWORK_OPTIONS,
            12
        )],
        "spec 13.3/13.4: callback serialization is per instance - an idle plugin is not blocked \
         by a busy one"
    );
    coalescer.end_callback(&idle, CallbackOutcome::Completed, 13);

    coalescer.post_event(&busy, LegacyEventFlags::PACKAGE_CONFIG, 14);
    assert!(
        coalescer.tick(14).is_empty(),
        "spec 13.4: an event must not interleave with the in-flight on_suggest callback"
    );
    let held = LegacyEventFlags::APP_CONFIG | LegacyEventFlags::PACKAGE_CONFIG;
    assert_eq!(
        coalescer.pending_flags(&busy),
        held,
        "spec 14.6: events that arrive during a callback accumulate into one pending set"
    );
    assert_eq!(
        coalescer.next_wakeup(),
        None,
        "spec 14.6: the deferral is callback serialization, not a debounce deadline"
    );

    coalescer.end_callback(&busy, CallbackOutcome::Completed, 20);
    assert_eq!(
        shape(&coalescer.tick(20)),
        vec![("legacy.busy", LegacyCallback::OnEvents, held, 20)],
        "spec 13.4/14.6: the held events are delivered as one union as soon as the instance is \
         free, at the tick that observes it"
    );

    let diagnostics = coalescer.diagnostics();
    assert_eq!(
        diagnostics.deferred_by_serialization, 2,
        "spec 26.2: both notices that arrived during the in-flight callback are diagnosable"
    );
    assert_eq!(
        diagnostics.events_delivered, 2,
        "spec 13.4: one delivery for the idle plugin, one for the busy one"
    );
}

// ---------------------------------------------------------------------------
// Plugin failure isolation (spec 26.2, 31.9)
// ---------------------------------------------------------------------------

#[test]
fn a_plugin_that_raises_inside_on_events_still_receives_later_events() {
    // Spec 31.9 + 26.2: a failing plugin callback is diagnosed, never fatal and
    // never sticky. The instance must not be left marked busy, and the flags of
    // the failed delivery must not be silently re-queued onto the next one -
    // that would make a raising plugin see phantom events forever.
    let plugin = plugin("legacy.raises");
    let mut coalescer = coalescer(&[&plugin]);

    coalescer.post_event(&plugin, LegacyEventFlags::FILESYSTEM, 0);
    assert_eq!(
        shape(&coalescer.tick(0)),
        vec![(
            "legacy.raises",
            LegacyCallback::OnEvents,
            LegacyEventFlags::FILESYSTEM,
            0
        )],
        "spec 14.6: the first event is delivered normally"
    );
    coalescer.end_callback(&plugin, CallbackOutcome::Raised, 1);
    assert_eq!(
        coalescer.pending_flags(&plugin),
        LegacyEventFlags::empty(),
        "spec 26.2: a raising callback is diagnosed, not silently retried"
    );

    coalescer.post_event(&plugin, LegacyEventFlags::NETWORK_OPTIONS, 5);
    assert_eq!(
        shape(&coalescer.tick(5)),
        vec![(
            "legacy.raises",
            LegacyCallback::OnEvents,
            LegacyEventFlags::NETWORK_OPTIONS,
            5
        )],
        "spec 31.9: a raise inside on_events must not poison the coalescer, and must not leave \
         the instance marked busy"
    );
    coalescer.end_callback(&plugin, CallbackOutcome::Completed, 6);

    coalescer.post_event(&plugin, LegacyEventFlags::APP_CONFIG, 7);
    assert_eq!(
        shape(&coalescer.tick(7)),
        vec![(
            "legacy.raises",
            LegacyCallback::OnEvents,
            LegacyEventFlags::APP_CONFIG,
            7
        )],
        "spec 31.9: event delivery continues indefinitely after a failed callback"
    );
    coalescer.end_callback(&plugin, CallbackOutcome::Completed, 8);

    let diagnostics = coalescer.diagnostics();
    assert_eq!(
        diagnostics.failed_deliveries, 1,
        "spec 26.2: the failure is reported exactly once as a compatibility diagnostic"
    );
    assert_eq!(
        diagnostics.events_delivered, 3,
        "spec 31.9: no event was lost around the failure"
    );
    assert_eq!(
        diagnostics.flag_unions_merged, 0,
        "spec 26.2: the failed delivery's flags must not be folded into later deliveries"
    );
}

// ---------------------------------------------------------------------------
// Per-plugin isolation (spec 13.3, 14.6)
// ---------------------------------------------------------------------------

#[test]
fn pending_event_flags_are_tracked_independently_per_plugin() {
    // Spec 13.3 + 14.6: pending flags belong to a plugin *instance*. One
    // plugin's accumulated set must never leak into another plugin's callback,
    // including while the first instance is blocked behind a long callback.
    let blocked = plugin("legacy.blocked");
    let quick = plugin("legacy.quick");
    let other = plugin("legacy.other");
    let mut coalescer = coalescer(&[&blocked, &quick, &other]);

    coalescer.begin_callback(&blocked, LegacyCallback::OnCatalog, 0);
    coalescer.post_event(&blocked, LegacyEventFlags::APP_CONFIG, 1);
    coalescer.post_event(&quick, LegacyEventFlags::NETWORK_OPTIONS, 1);
    coalescer.post_event(&blocked, LegacyEventFlags::PACKAGE_CONFIG, 2);
    coalescer.post_event(&other, LegacyEventFlags::DESKTOP, 2);

    let blocked_flags = LegacyEventFlags::APP_CONFIG | LegacyEventFlags::PACKAGE_CONFIG;
    assert_eq!(
        coalescer.pending_flags(&blocked),
        blocked_flags,
        "spec 14.6: the blocked instance accumulates only its own flags"
    );
    assert_eq!(
        coalescer.pending_flags(&quick),
        LegacyEventFlags::NETWORK_OPTIONS,
        "spec 14.6: pending flags are per instance"
    );

    assert_eq!(
        shape(&coalescer.tick(3)),
        vec![
            (
                "legacy.other",
                LegacyCallback::OnEvents,
                LegacyEventFlags::DESKTOP,
                3
            ),
            (
                "legacy.quick",
                LegacyCallback::OnEvents,
                LegacyEventFlags::NETWORK_OPTIONS,
                3
            ),
        ],
        "spec 13.3/14.6: each plugin receives exactly the flags posted to it, and never another \
         plugin's pending set"
    );
    coalescer.end_callback(&quick, CallbackOutcome::Completed, 4);
    coalescer.end_callback(&other, CallbackOutcome::Completed, 4);
    assert_eq!(
        coalescer.pending_flags(&quick),
        LegacyEventFlags::empty(),
        "spec 14.6: delivering one instance clears only that instance"
    );
    assert_eq!(
        coalescer.pending_flags(&blocked),
        blocked_flags,
        "spec 13.4: the blocked instance keeps its pending set while its callback runs"
    );

    coalescer.end_callback(&blocked, CallbackOutcome::Completed, 5);
    assert_eq!(
        shape(&coalescer.tick(5)),
        vec![("legacy.blocked", LegacyCallback::OnEvents, blocked_flags, 5)],
        "spec 14.6: no flag from another plugin's pending set may leak into this callback"
    );
    coalescer.end_callback(&blocked, CallbackOutcome::Completed, 6);

    assert_eq!(
        coalescer.diagnostics().events_delivered,
        3,
        "spec 13.3: three instances, three independent deliveries"
    );
}

// ---------------------------------------------------------------------------
// Empty deliveries (spec 14.6)
// ---------------------------------------------------------------------------

#[test]
fn an_empty_flag_set_is_never_delivered_as_a_callback() {
    // Spec 14.6: `on_events(flags)` describes what changed. A callback carrying
    // no flags tells a plugin nothing and forces it to re-read everything, so
    // an empty set is discarded at the source rather than delivered.
    let plugin = plugin("legacy.quiet");
    let mut coalescer = coalescer(&[&plugin]);

    coalescer.post_event(&plugin, LegacyEventFlags::empty(), 0);
    assert_eq!(
        coalescer.pending_flags(&plugin),
        LegacyEventFlags::empty(),
        "spec 14.6: an empty notice adds nothing to the pending set"
    );
    assert!(
        coalescer.tick(0).is_empty(),
        "spec 14.6: an empty flag set is never delivered as an on_events callback"
    );

    coalescer.broadcast_event(LegacyEventFlags::empty(), 1);
    assert!(
        coalescer.tick(1).is_empty(),
        "spec 14.6: broadcasting an empty flag set delivers nothing to anyone"
    );
    assert_eq!(
        coalescer.next_wakeup(),
        None,
        "spec 14.6: a discarded empty notice must not arm a deadline"
    );

    coalescer.broadcast_event(LegacyEventFlags::PACKAGES, 2);
    assert_eq!(
        shape(&coalescer.tick(2)),
        vec![(
            "legacy.quiet",
            LegacyCallback::OnEvents,
            LegacyEventFlags::PACKAGES,
            2
        )],
        "spec 14.6: discarded empty notices must not disturb a real event that follows"
    );
    coalescer.end_callback(&plugin, CallbackOutcome::Completed, 3);

    assert!(
        coalescer.tick(3).is_empty(),
        "spec 14.6: an emptied pending set must not produce a trailing empty callback"
    );
    assert!(
        coalescer.tick(4).is_empty(),
        "spec 14.6: idle ticks never deliver"
    );

    let diagnostics = coalescer.diagnostics();
    assert_eq!(
        diagnostics.empty_events_discarded, 2,
        "spec 26.2: both the direct post and the broadcast are counted as discarded"
    );
    assert_eq!(
        diagnostics.events_delivered, 1,
        "spec 14.6: only the non-empty event reached the plugin"
    );
}

#[test]
fn an_out_of_order_raw_notification_keeps_the_burst_deadline_bounded_by_its_earliest_timestamp() {
    let plugin = plugin("legacy.out-of-order");
    let mut coalescer = coalescer(&[&plugin]);

    coalescer.note_raw_filesystem(
        raw(
            WatchScope::Desktop,
            "/desktop/late-first",
            RawFilesystemKind::Modified,
        ),
        1_000,
    );
    coalescer.note_raw_filesystem(
        raw(
            WatchScope::Desktop,
            "/desktop/early-second",
            RawFilesystemKind::Modified,
        ),
        0,
    );

    assert_eq!(
        coalescer.next_wakeup(),
        Some(200),
        "spec 18.7: maximum wait is measured from the earliest timestamp in a burst, \
         even when watcher notifications arrive out of order",
    );
    assert!(
        coalescer.tick(199).is_empty(),
        "the bounded deadline has not elapsed yet",
    );
    assert_eq!(
        shape(&coalescer.tick(200)),
        vec![(
            "legacy.out-of-order",
            LegacyCallback::OnEvents,
            LegacyEventFlags::DESKTOP | LegacyEventFlags::FILESYSTEM,
            200,
        )],
        "the out-of-order burst still translates once",
    );
}

#[test]
fn a_duplicate_begin_callback_cannot_replace_the_callback_already_in_flight() {
    let plugin = plugin("legacy.duplicate-callback");
    let mut coalescer = coalescer(&[&plugin]);

    coalescer.begin_callback(&plugin, LegacyCallback::OnSuggest, 10);
    coalescer.begin_callback(&plugin, LegacyCallback::OnCatalog, 20);

    assert_eq!(
        coalescer.in_flight(&plugin),
        Some((LegacyCallback::OnSuggest, 10)),
        "spec 13.4: a duplicate start must not forget which callback is still running",
    );

    coalescer.post_event(&plugin, LegacyEventFlags::APP_CONFIG, 21);
    assert!(
        coalescer.tick(21).is_empty(),
        "spec 13.4: replacing the in-flight record would allow event interleaving",
    );
    coalescer.end_callback(&plugin, CallbackOutcome::Completed, 30);
    assert_eq!(
        shape(&coalescer.tick(30)),
        vec![(
            "legacy.duplicate-callback",
            LegacyCallback::OnEvents,
            LegacyEventFlags::APP_CONFIG,
            30,
        )],
        "the pending event becomes deliverable after the original callback ends",
    );
}

#[test]
fn unregistering_a_plugin_discards_queued_events_before_the_next_tick() {
    let plugin = plugin("legacy.unsubscribed");
    let mut coalescer = coalescer(&[&plugin]);

    coalescer.post_event(&plugin, LegacyEventFlags::APP_CONFIG, 0);
    coalescer.note_deactivated(&plugin, 0);
    coalescer.note_raw_filesystem(
        raw(
            WatchScope::Desktop,
            "/desktop/unsubscribed",
            RawFilesystemKind::Modified,
        ),
        0,
    );
    coalescer.unregister_plugin(&plugin);

    assert!(
        coalescer.tick(1_000).is_empty(),
        "spec 13.3/14.6: an unloaded plugin must not receive queued semantic or lifecycle events",
    );

    coalescer.register_plugin(plugin.clone());
    assert!(
        coalescer.tick(1_000).is_empty(),
        "re-registering the same id must not resurrect events queued before unsubscription",
    );
}
