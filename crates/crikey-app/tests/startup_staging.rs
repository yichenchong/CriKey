use crikey_app::{App, StartupError, StartupStage};

const STARTUP_ORDER: [StartupStage; 7] = [
    StartupStage::WindowAndHotkey,
    StartupStage::PersistedCatalog,
    StartupStage::AcceptQueries,
    StartupStage::RequiredWorkers,
    StartupStage::LegacyPlugins,
    StartupStage::BackgroundRefresh,
    StartupStage::LazyModernPlugins,
];

#[test]
fn fresh_app_starts_at_window_and_hotkey_without_accepting_queries() {
    let app = App::new();

    assert_eq!(app.stage(), StartupStage::WindowAndHotkey);
    assert!(!app.can_accept_queries());
    assert!(!app.startup_complete());
}

#[test]
fn startup_completes_stages_one_at_a_time_in_exact_spec_order() {
    let mut app = App::new();

    assert_eq!(app.stage(), STARTUP_ORDER[0]);

    for adjacent_stages in STARTUP_ORDER.windows(2) {
        let current = adjacent_stages[0];
        let next = adjacent_stages[1];

        assert_eq!(app.stage(), current);
        assert_eq!(app.complete_stage(current), Ok(Some(next)));
        assert_eq!(app.stage(), next);
    }

    assert_eq!(app.stage(), StartupStage::LazyModernPlugins);
    assert!(!app.startup_complete());
    assert_eq!(app.complete_stage(StartupStage::LazyModernPlugins), Ok(None));
    assert_eq!(app.stage(), StartupStage::LazyModernPlugins);
    assert!(app.startup_complete());
}

#[test]
fn replaying_every_intermediate_acknowledgement_is_stale_and_atomic() {
    let mut app = App::new();

    for adjacent_stages in STARTUP_ORDER.windows(2) {
        let completed = adjacent_stages[0];
        let pending = adjacent_stages[1];

        assert_eq!(app.complete_stage(completed), Ok(Some(pending)));
        let readiness = app.can_accept_queries();
        let startup_complete = app.startup_complete();

        assert_eq!(
            app.complete_stage(completed),
            Err(StartupError::StaleAcknowledgement {
                expected: completed,
                pending,
            })
        );
        assert_eq!(app.stage(), pending);
        assert_eq!(app.can_accept_queries(), readiness);
        assert_eq!(app.startup_complete(), startup_complete);
    }
}

#[test]
fn out_of_order_acknowledgements_leave_startup_state_unchanged() {
    let mut app = App::new();

    for expected in STARTUP_ORDER.into_iter().skip(1) {
        assert_eq!(
            app.complete_stage(expected),
            Err(StartupError::OutOfOrderAcknowledgement {
                expected,
                pending: StartupStage::WindowAndHotkey,
            })
        );
        assert_eq!(app.stage(), StartupStage::WindowAndHotkey);
        assert!(!app.can_accept_queries());
        assert!(!app.startup_complete());
    }
}

#[test]
fn query_acceptance_and_completion_follow_acknowledgement_boundaries() {
    let expected_pending_states = [
        (StartupStage::WindowAndHotkey, false),
        (StartupStage::PersistedCatalog, false),
        (StartupStage::AcceptQueries, false),
        (StartupStage::RequiredWorkers, true),
        (StartupStage::LegacyPlugins, true),
        (StartupStage::BackgroundRefresh, true),
        (StartupStage::LazyModernPlugins, true),
    ];
    let mut app = App::new();

    for (index, (pending, can_accept_queries)) in expected_pending_states.into_iter().enumerate() {
        assert_eq!(app.stage(), pending);
        assert_eq!(app.can_accept_queries(), can_accept_queries);
        assert!(!app.startup_complete());

        let next_pending = STARTUP_ORDER.get(index + 1).copied();
        assert_eq!(app.complete_stage(pending), Ok(next_pending));
    }

    assert_eq!(app.stage(), StartupStage::LazyModernPlugins);
    assert!(app.can_accept_queries());
    assert!(app.startup_complete());
}

#[test]
fn duplicate_final_acknowledgement_returns_already_complete_without_mutation() {
    let mut app = App::new();

    for (index, stage) in STARTUP_ORDER.into_iter().enumerate() {
        let next_pending = STARTUP_ORDER.get(index + 1).copied();
        assert_eq!(app.complete_stage(stage), Ok(next_pending));
    }

    for _ in 0..3 {
        assert_eq!(
            app.complete_stage(StartupStage::LazyModernPlugins),
            Err(StartupError::AlreadyComplete)
        );
        assert_eq!(app.stage(), StartupStage::LazyModernPlugins);
        assert!(app.can_accept_queries());
        assert!(app.startup_complete());
    }
}
