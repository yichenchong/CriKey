use super::*;
use crikey_platform::Accelerator;

fn chord(value: &str) -> Accelerator {
    Accelerator::parse(value).unwrap()
}

fn context(scope: NavigationScope) -> NavigationContext {
    NavigationContext {
        scope,
        owner: NavigationOwner::Query,
        composing: false,
    }
}

#[test]
fn profiles_keep_text_and_protected_owners_out_of_host_navigation() {
    for style in [
        NavigationStyle::Default,
        NavigationStyle::Emacs,
        NavigationStyle::Vim,
    ] {
        let resolver = CompiledNavigation::compile(style, "{}").unwrap();
        for text in ["J", "K", "Shift+J", "Shift+K", "Space", "Shift+Space"] {
            assert_eq!(
                resolver.resolve(chord(text), context(NavigationScope::Results)),
                None
            );
        }
        for owner in [
            NavigationOwner::Editor,
            NavigationOwner::Password,
            NavigationOwner::Popup,
            NavigationOwner::Document,
            NavigationOwner::Web,
            NavigationOwner::ShortcutCapture,
        ] {
            for binding in resolver.bindings() {
                assert_eq!(
                    resolver.resolve(
                        binding.chord,
                        NavigationContext {
                            owner,
                            ..context(NavigationScope::Standard)
                        }
                    ),
                    None
                );
            }
        }
        for binding in resolver.bindings() {
            assert_eq!(
                resolver.resolve(
                    binding.chord,
                    NavigationContext {
                        composing: true,
                        ..context(NavigationScope::Standard)
                    }
                ),
                None
            );
        }
    }
}

#[test]
fn exact_chords_and_scopes_share_dynamic_hints() {
    let resolver = CompiledNavigation::compile(NavigationStyle::Vim, "{}").unwrap();
    let results = context(NavigationScope::Results);
    assert_eq!(
        resolver.resolve(chord("alt+j"), results),
        Some(NavigationAction::Next)
    );
    assert_eq!(resolver.resolve(chord("Alt+Shift+J"), results), None);
    assert_eq!(
        resolver
            .hints(NavigationAction::Next, results)
            .collect::<Vec<_>>(),
        ["Down", "Alt+J"]
    );
    assert_eq!(resolver.resolve(chord("Left"), results), None);
    assert_eq!(
        resolver.resolve(chord("Left"), context(NavigationScope::Standard)),
        None
    );
    assert_eq!(
        resolver.resolve(
            chord("Left"),
            NavigationContext {
                owner: NavigationOwner::Host,
                ..context(NavigationScope::Standard)
            }
        ),
        Some(NavigationAction::Left)
    );
    assert_eq!(
        resolver
            .hints(NavigationAction::Left, context(NavigationScope::Standard))
            .count(),
        0
    );
    assert_eq!(
        resolver
            .hints(
                NavigationAction::Left,
                NavigationContext {
                    owner: NavigationOwner::Host,
                    ..context(NavigationScope::Standard)
                }
            )
            .collect::<Vec<_>>(),
        ["Left"]
    );
    assert_eq!(
        resolver.resolve(chord("Enter"), context(NavigationScope::Settings)),
        None
    );
    assert_eq!(
        resolver.resolve(chord("Tab"), context(NavigationScope::Page)),
        None
    );
    assert_eq!(
        resolver.resolve(chord("Escape"), context(NavigationScope::Settings)),
        Some(NavigationAction::Cancel)
    );
}

#[test]
fn atomic_overrides_allow_swaps_disable_and_canonical_conflict_checks() {
    let resolver = CompiledNavigation::compile(
        NavigationStyle::Default,
        r#"{"next":["Up"],"previous":["Down"],"show-actions":[]}"#,
    )
    .unwrap();
    assert_eq!(
        resolver.resolve(chord("Up"), context(NavigationScope::Results)),
        Some(NavigationAction::Next)
    );
    assert_eq!(
        resolver.resolve(chord("Ctrl+K"), context(NavigationScope::Results)),
        None
    );
    for source in [
        ShortcutSource::DesiredActivation,
        ShortcutSource::SavedCommand,
        ShortcutSource::AppliedActivation,
        ShortcutSource::AppliedCommand,
    ] {
        let conflict = resolver.conflict(chord("  up  "), source).unwrap();
        assert_eq!(conflict.action, NavigationAction::Next);
        assert_eq!(conflict.source, source);
        assert_eq!(conflict.chord, chord("Up"));
    }
    assert_eq!(
        resolver.conflict(chord("Ctrl+Alt+Space"), ShortcutSource::AppliedActivation),
        None
    );
}

#[test]
fn invalid_or_ambiguous_overrides_are_rejected_as_a_whole() {
    for json in [
        r#"{"unknown":["Ctrl+N"]}"#,
        r#"{"next":["Ctrl+N"],"next":["Ctrl+P"]}"#,
        r#"{"next":["Up"]}"#,
        r#"{"next":["Ctrl+N","ctrl+n"]}"#,
        r#"{"next":["J"]}"#,
        r#"{"next":["Shift+K"]}"#,
        r#"{"next":["Space"]}"#,
        r#"{"next":["Ctrl+NotAKey"]}"#,
        r#"{"next":["Ctrl+N","Ctrl+P","Ctrl+J","Ctrl+L","Ctrl+H"]}"#,
        "[]",
        "null",
        r#"{"next":"Ctrl+N"}"#,
        "{} {}",
    ] {
        assert!(
            CompiledNavigation::compile(NavigationStyle::Default, json).is_err(),
            "{json}"
        );
    }
    let oversized = " ".repeat(MAX_NAVIGATION_BINDINGS_BYTES + 1);
    assert_eq!(
        CompiledNavigation::compile(NavigationStyle::Default, &oversized),
        Err(NavigationError::Limit)
    );
    let long_chord = format!(
        r#"{{"next":["{}Ctrl+N"]}}"#,
        " ".repeat(MAX_NAVIGATION_CHORD_BYTES)
    );
    assert!(CompiledNavigation::compile(NavigationStyle::Default, &long_chord).is_err());
    assert!(NavigationStyle::parse("unknown").is_err());
    assert!(CompiledNavigation::compile(NavigationStyle::Default, "").is_ok());
}

#[test]
fn remapped_submission_and_cancel_still_respect_ownership() {
    let resolver = CompiledNavigation::compile(
        NavigationStyle::Default,
        r#"{"activate":["Ctrl+L"],"cancel":["Ctrl+Q"],"complete":["Ctrl+T"]}"#,
    )
    .unwrap();
    for (key, action) in [
        ("Ctrl+L", NavigationAction::Activate),
        ("Ctrl+Q", NavigationAction::Cancel),
        ("Ctrl+T", NavigationAction::Complete),
    ] {
        assert_eq!(
            resolver.resolve(chord(key), context(NavigationScope::Results)),
            Some(action)
        );
        for owner in [
            NavigationOwner::Editor,
            NavigationOwner::Password,
            NavigationOwner::Popup,
            NavigationOwner::Document,
            NavigationOwner::Web,
        ] {
            assert_eq!(
                resolver.resolve(
                    chord(key),
                    NavigationContext {
                        owner,
                        ..context(NavigationScope::Results)
                    }
                ),
                None
            );
        }
        assert_eq!(
            resolver.resolve(
                chord(key),
                NavigationContext {
                    composing: true,
                    ..context(NavigationScope::Results)
                }
            ),
            None
        );
    }
    assert_eq!(
        resolver.resolve(chord("Enter"), context(NavigationScope::Actions)),
        None
    );
}

#[test]
fn scalar_validation_does_not_assume_profile_defaults() {
    let json = r#"{"previous":["Down"]}"#;
    assert_eq!(validate_bindings_json(json), Ok(()));
    assert!(CompiledNavigation::compile(NavigationStyle::Default, json).is_err());
    for invalid in [
        r#"{"next":["Shift+J"]}"#,
        r#"{"next":["Ctrl+Unknown"]}"#,
        r#"{"next":[],"next":[]}"#,
        r#"{"unknown":[]}"#,
        "[]",
    ] {
        assert!(validate_bindings_json(invalid).is_err());
    }
}

#[test]
fn default_command_modifier_matches_target_platform() {
    let resolver = CompiledNavigation::compile(NavigationStyle::Default, "{}").unwrap();
    let (command, other) = if cfg!(target_os = "macos") {
        ("Meta+K", "Ctrl+K")
    } else {
        ("Ctrl+K", "Meta+K")
    };
    let results = context(NavigationScope::Results);
    assert_eq!(
        resolver.resolve(chord(command), results),
        Some(NavigationAction::ShowActions)
    );
    assert_eq!(resolver.resolve(chord(other), results), None);
    for (key, action) in [
        ("Enter", NavigationAction::Activate),
        ("Tab", NavigationAction::Complete),
        ("Escape", NavigationAction::Cancel),
    ] {
        assert_eq!(resolver.resolve(chord(key), results), Some(action));
    }
}

#[test]
fn disjoint_scopes_can_reuse_a_chord_without_dispatch_ambiguity() {
    let resolver = CompiledNavigation::compile(NavigationStyle::Default, r#"{"complete":["Left"]}"#).unwrap();
    let host = |scope| NavigationContext {
        owner: NavigationOwner::Host,
        ..context(scope)
    };
    assert_eq!(
        resolver.resolve(chord("Left"), host(NavigationScope::Results)),
        Some(NavigationAction::Complete)
    );
    assert_eq!(
        resolver.resolve(chord("Left"), host(NavigationScope::Standard)),
        Some(NavigationAction::Left)
    );
    assert_eq!(
        resolver
            .hints(NavigationAction::Complete, host(NavigationScope::Results))
            .collect::<Vec<_>>(),
        ["Left"]
    );
    assert_eq!(
        resolver
            .hints(NavigationAction::Left, host(NavigationScope::Standard))
            .collect::<Vec<_>>(),
        ["Left"]
    );
    assert!(CompiledNavigation::compile(NavigationStyle::Default, r#"{"complete":["Down"]}"#).is_err());
    assert!(
        CompiledNavigation::compile(NavigationStyle::Default, r#"{"complete":["Left","left"]}"#).is_err()
    );
}

#[test]
fn actually_held_grabs_filter_runtime_actions_and_hints_without_losing_desired_map() {
    let desired = CompiledNavigation::compile(NavigationStyle::Vim, r#"{"complete":["Left"]}"#).unwrap();
    let held = [chord("alt+j"), chord("Left"), chord("Ctrl+Alt+Space")];
    let applied = desired.without_chords(&held);
    let results = context(NavigationScope::Results);
    assert_eq!(
        desired.resolve(chord("Alt+J"), results),
        Some(NavigationAction::Next)
    );
    assert_eq!(applied.resolve(chord("Alt+J"), results), None);
    assert_eq!(
        applied.hints(NavigationAction::Next, results).collect::<Vec<_>>(),
        ["Down"]
    );
    assert_eq!(applied.resolve(chord("Left"), results), None);
    assert_eq!(
        applied.resolve(chord("Left"), context(NavigationScope::Standard)),
        None
    );
    assert_eq!(applied.hints(NavigationAction::Complete, results).count(), 0);
    assert_eq!(
        applied
            .hints(NavigationAction::Left, context(NavigationScope::Standard))
            .count(),
        0
    );
    assert_eq!(
        applied.resolve(chord("Down"), results),
        Some(NavigationAction::Next)
    );
    assert!(desired
        .conflict(chord("Alt+J"), ShortcutSource::AppliedCommand)
        .is_some());
    assert!(applied
        .conflict(chord("Alt+J"), ShortcutSource::AppliedCommand)
        .is_none());
    assert_eq!(desired.without_chords(&[]), desired);
}

#[test]
fn query_editing_chords_cannot_be_stolen_by_navigation_overrides() {
    let command = if cfg!(target_os = "macos") { "Meta" } else { "Ctrl" };
    let mut editing = vec![
        "Left".to_owned(),
        "Shift+Right".to_owned(),
        "Home".to_owned(),
        "Shift+End".to_owned(),
        "Ctrl+Left".to_owned(),
        "Alt+Shift+Right".to_owned(),
        "Meta+Left".to_owned(),
        "Backspace".to_owned(),
        "Alt+Backspace".to_owned(),
        "Ctrl+Delete".to_owned(),
        "Shift+Insert".to_owned(),
        "Ctrl+H".to_owned(),
        "Ctrl+U".to_owned(),
        "Ctrl+W".to_owned(),
        "Ctrl+K".to_owned(),
    ];
    for key in ["A", "C", "V", "X", "Z", "Y"] {
        editing.push(format!("{command}+{key}"));
        editing.push(format!("{command}+Shift+{key}"));
    }
    if cfg!(target_os = "macos") {
        for key in ["A", "B", "E", "F"] {
            editing.push(format!("Ctrl+{key}"));
        }
    }
    for key in editing {
        let json = format!(r#"{{"next":["{key}"],"left":[],"right":[],"show-actions":[]}}"#);
        let resolver = CompiledNavigation::compile(NavigationStyle::Default, &json).unwrap();
        let query = context(NavigationScope::Results);
        let host = NavigationContext {
            owner: NavigationOwner::Host,
            ..query
        };
        assert_eq!(resolver.resolve(chord(&key), query), None, "{key}");
        assert_eq!(resolver.hints(NavigationAction::Next, query).count(), 0, "{key}");
        assert_eq!(
            resolver.resolve(chord(&key), host),
            Some(NavigationAction::Next),
            "{key}"
        );
        assert_eq!(resolver.hints(NavigationAction::Next, host).count(), 1, "{key}");
    }
    let disjoint = CompiledNavigation::compile(NavigationStyle::Default, r#"{"complete":["Left"]}"#).unwrap();
    assert_eq!(
        disjoint.resolve(chord("Left"), context(NavigationScope::Results)),
        None
    );
    assert_eq!(
        disjoint
            .hints(NavigationAction::Complete, context(NavigationScope::Results))
            .count(),
        0
    );
}

#[test]
fn invalid_chord_errors_do_not_retain_submitted_components() {
    for json in [
        r#"{"next":["Ctrl+private-canary-value"]}"#,
        r#"{"next":["Ctrl+N+private-canary-value"]}"#,
    ] {
        let error = CompiledNavigation::compile(NavigationStyle::Default, json).unwrap_err();
        assert_eq!(error, NavigationError::InvalidChord);
        assert!(!error.to_string().contains("private-canary-value"));
        assert!(!format!("{error:?}").contains("private-canary-value"));
        let scalar_error = validate_bindings_json(json).unwrap_err();
        assert_eq!(scalar_error, NavigationError::InvalidChord);
    }
}
