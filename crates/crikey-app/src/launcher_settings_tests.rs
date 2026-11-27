use super::*;
use crate::settings_search::{SettingsSearch, SettingsSearchError};
use crikey_config::{ConfigLayer, DiscoveredSchema};
use crikey_core::PluginId;
use crikey_platform::{DirectoryConvention, DirectoryEnvironment, StandardDirectories};
use crikey_plugin_model::{ConfigurationField, ConfigurationKind, ConfigurationSection};

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("crikey-launcher-metadata-{}-{id}", std::process::id()));
        std::fs::create_dir(&path).expect("private fixture directory");
        Self(path)
    }
    fn store(&self) -> ConfigStore {
        let environment = [
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "CRIKEY_CONFIG_DIR",
            "CRIKEY_DATA_DIR",
            "CRIKEY_CACHE_DIR",
            "CRIKEY_STATE_DIR",
        ]
        .into_iter()
        .fold(DirectoryEnvironment::new(), |env, key| {
            env.set(key, self.0.as_os_str())
        });
        let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment).unwrap();
        ConfigStore::load_with_policy(&directories, None).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn field(name: &str, secret: bool) -> ConfigurationField {
    ConfigurationField {
        name: name.into(),
        kind: ConfigurationKind::String,
        default: Some(
            if secret {
                "SECRET_DEFAULT"
            } else {
                "PUBLIC_DEFAULT_NOT_METADATA"
            }
            .into(),
        ),
        description: if secret {
            "SECRET_DESCRIPTION"
        } else {
            "Visible declaration"
        }
        .into(),
        secret,
        requires_restart: true,
        platforms: Vec::new(),
        required: false,
        minimum: None,
        maximum: None,
        max_length: Some(40),
        allowed: vec![
            if secret {
                "SECRET_DEFAULT"
            } else {
                "PUBLIC_DEFAULT_NOT_METADATA"
            }
            .into(),
            if secret {
                "SECRET_ALLOWED"
            } else {
                "PUBLIC_ALLOWED_NOT_METADATA"
            }
            .into(),
        ],
    }
}

#[test]
fn one_declaration_drives_panel_navigation_and_search() {
    let definitions = search_definitions();
    let search = SettingsSearch::new(3, &definitions, &[], "linux").unwrap();
    let panel = rows(None);
    assert_eq!(panel.len(), LAUNCHER_SETTINGS.len());
    let mut keys = std::collections::BTreeSet::new();
    for ((declaration, row), definition) in LAUNCHER_SETTINGS.iter().zip(&panel).zip(&definitions) {
        assert!(keys.insert(declaration.key), "duplicate declaration");
        assert_eq!(row.key, definition.key);
        assert_eq!(row.label, definition.label);
        let hits = search.search_owned(declaration.key, 3).unwrap();
        let hit = hits.hits.iter().find(|hit| hit.key == row.key).unwrap();
        assert_eq!(hit.navigation_target, declaration.navigation_target);
        assert_eq!(hit.requires_restart, declaration.restart == Restart::Required);
        assert_eq!(
            matches!(row.control, SettingControl::Toggle { .. }),
            declaration.validator == Validator::Boolean
        );
    }
    // Whitespace tokens may match different metadata fields, in either order.
    assert_eq!(
        search.search_owned("APPEARANCE footer", 3).unwrap().hits[0].key,
        KEY_SHOW_HINTS
    );
    assert!(search
        .search_owned("footer missingword", 3)
        .unwrap()
        .hits
        .is_empty());
}

#[test]
fn validator_boundaries_reject_unusable_values_without_restricting_valid_names() {
    for (key, valid, invalid) in [
        (KEY_ACTIVATION_HOTKEY, vec!["Ctrl+Alt+Space"], vec!["Ctrl+"]),
        (KEY_MAX_RESULTS, vec!["1", "100"], vec!["0", "-1", "1.5"]),
        (
            KEY_PROFILE,
            vec!["work", "true", "two words", ".work"],
            vec![".", "..", "../work", "a\\b"],
        ),
        (
            KEY_SHOW_HINTS,
            vec!["true", "false"],
            vec!["False", "yes", " true"],
        ),
        (KEY_ROUNDED_CORNERS, vec!["true", "false"], vec!["TRUE", "0"]),
        (
            KEY_PREVIEW_SPLIT,
            vec!["0.3", "0.7"],
            vec!["0.299", "0.701", "NaN", "inf"],
        ),
        (
            "launcher.resume-delay-ms",
            vec!["0", "18446744073709551615"],
            vec!["-1", "18446744073709551616", "1.0"],
        ),
        (
            "launcher.query-history",
            vec!["off", "session", "persistent"],
            vec!["ON", "persistent "],
        ),
        (
            "launcher.command-home",
            vec!["compact", "favorites", "recent", "favorites-and-recent"],
            vec!["all", "favorites_and_recent"],
        ),
    ] {
        for value in valid {
            assert!(validate(key, value).is_ok(), "{key}={value}");
        }
        for value in invalid {
            assert!(validate(key, value).is_err(), "{key}={value}");
        }
        assert!(validate(key, "").is_err());
    }
    assert!(validate("launcher.unknown", "true").is_err());
}

#[test]
fn appearance_and_navigation_use_canonical_parsers_and_explicit_invalid_projection() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    assert!(configured_appearance(None).is_ok());
    assert!(configured_navigation(None).is_ok());
    for (key, value) in [
        (KEY_THEME, "sepia"),
        (KEY_INTERFACE_SIZE, "huge"),
        (KEY_RESULT_DENSITY, "dense"),
        (KEY_THEME_OVERRIDES, r##"{"unknown":"#123456"}"##),
        (KEY_THEME_OVERRIDES, r##"{"text":"#ffffff","text":"#000000"}"##),
        (KEY_NAVIGATION_STYLE, "custom"),
        (KEY_NAVIGATION_BINDINGS, r#"{"unknown":["Ctrl+Alt+J"]}"#),
        (KEY_NAVIGATION_BINDINGS, r#"{"next":["j"]}"#),
        (KEY_NAVIGATION_BINDINGS, "[]"),
    ] {
        assert!(validate(key, value).is_err(), "{key}={value}");
    }
    let unreadable = r##"{"canvas":"#111111","text":"#111111"}"##;
    // Syntax alone cannot establish cross-role readability.
    assert!(validate(KEY_THEME_OVERRIDES, unreadable).is_ok());
    assert!(validate_edit(&store, KEY_THEME_OVERRIDES, unreadable).is_err());
    assert_eq!(store.get(KEY_THEME_OVERRIDES), Some("{}"));
    store.set_session_override(KEY_THEME_OVERRIDES, unreadable);
    assert!(configured_appearance(Some(&store)).is_err());
    assert_eq!(
        declaration(KEY_THEME_OVERRIDES)
            .unwrap()
            .read(Some(&store))
            .source,
        "session-override"
    );
    assert_eq!(
        declaration(KEY_THEME_OVERRIDES).unwrap().read(Some(&store)).value,
        unreadable
    );
    store.set_session_override(
        KEY_NAVIGATION_BINDINGS,
        r#"{"next":["Ctrl+Alt+J"],"previous":["Ctrl+Alt+J"]}"#,
    );
    assert!(configured_navigation(Some(&store)).is_err());
}

#[test]
fn candidate_shortcut_conflicts_include_session_hidden_restart_preferences() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    store.set_user_global(KEY_ACTIVATION_HOTKEY, "Ctrl+Alt+J");
    store.save().unwrap();
    store.set_session_override(KEY_NAVIGATION_BINDINGS, "{}");
    let draft = r#"{"next":["Ctrl+Alt+J"]}"#;
    let error = validate_edit(&store, KEY_NAVIGATION_BINDINGS, draft).unwrap_err();
    assert!(error.contains("Restart configuration"), "{error}");
    assert!(error.contains("activation"), "{error}");
    assert_eq!(store.get(KEY_NAVIGATION_BINDINGS), Some("{}"));
    let preferences = crikey_config::command_preferences::CommandPreferences {
        records: vec![crikey_config::command_preferences::CommandPreference {
            reference: crikey_core::command::builtin_commands().remove(0).reference,
            hidden: false,
            alias: None,
            hotkey: Some("Ctrl+Alt+K".into()),
        }],
        ..Default::default()
    };
    store.set_session_override(
        crikey_config::command_preferences::KEY_COMMAND_PREFERENCES,
        &preferences.encode().unwrap(),
    );
    store.set_session_override(KEY_NAVIGATION_BINDINGS, r#"{"next":["Ctrl+Alt+K"]}"#);
    let error = validate_configuration(&store).unwrap_err();
    assert!(error.contains("Saved command"), "{error}");
    store.set_session_override(KEY_NAVIGATION_BINDINGS, "{}");
    assert!(validate_configuration(&store).is_ok());
}

#[test]
fn closed_preferences_expose_live_choices_and_builtin_provenance() {
    let fixture = Fixture::new();
    let store = fixture.store();
    for (key, options) in [
        (KEY_THEME, THEME_OPTIONS),
        (KEY_INTERFACE_SIZE, INTERFACE_SIZE_OPTIONS),
        (KEY_RESULT_DENSITY, RESULT_DENSITY_OPTIONS),
        (KEY_NAVIGATION_STYLE, NAVIGATION_STYLE_OPTIONS),
    ] {
        let declaration = declaration(key).unwrap();
        assert_eq!(declaration.restart, Restart::Live);
        assert_eq!(
            declaration.read(Some(&store)).source,
            ConfigLayer::BuiltInDefaults.as_str()
        );
        assert_eq!(
            declaration.row(Some(&store)).control,
            SettingControl::Choice { options }
        );
        for (value, _) in options {
            assert!(declaration.validate(value).is_ok(), "{key}={value}");
        }
    }
}

#[test]
fn independent_preference_groups_do_not_block_each_others_repairs() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    store.set_user_global(KEY_THEME_OVERRIDES, r##"{"canvas":"#111111","text":"#111111"}"##);
    store.set_user_global(KEY_NAVIGATION_BINDINGS, r#"{"next":["Ctrl+Alt+Space"]}"#);
    store.save().unwrap();
    assert!(validate_edit(&store, KEY_THEME_OVERRIDES, "{}").is_ok());
    assert!(validate_edit(&store, KEY_NAVIGATION_BINDINGS, "{}").is_ok());
}

#[test]
fn prospective_profile_change_after_validation_refuses_persistence() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    store.set_user_global(KEY_THEME, "dark");
    store.save().unwrap();
    let profiles = fixture.0.join(crikey_config::PROFILE_DIRECTORY);
    std::fs::create_dir_all(&profiles).unwrap();
    let prospective = profiles.join("next.toml");
    std::fs::write(&prospective, "[launcher]\ntheme = \"light\"\n").unwrap();
    let receipts = validate_edit(&store, KEY_PROFILE, "next").unwrap();
    let expected = store.revision();
    let before = std::fs::read(store.config_path()).unwrap();
    std::fs::write(&prospective, "[launcher]\ntheme = \"invalid\"\n").unwrap();
    assert!(matches!(
        store.commit_user_global_checked(KEY_PROFILE, "next", &expected, &receipts),
        Err(crikey_config::ConfigError::Conflict)
    ));
    assert_eq!(store.revision(), expected);
    assert_eq!(store.get(KEY_PROFILE), None);
    assert_eq!(std::fs::read(store.config_path()).unwrap(), before);
}

#[test]
fn profile_selector_validates_the_selected_profile_not_the_previous_layer() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let profiles = fixture.0.join(crikey_config::PROFILE_DIRECTORY);
    std::fs::create_dir_all(&profiles).unwrap();
    for (profile, contents, reason) in [
        ("invalid-theme", "[launcher]\ntheme = \"sepia\"\n", "theme"),
        (
            "invalid-navigation",
            "[launcher]\nnavigation-bindings = '{\"next\":[\"Ctrl+Alt+Space\"]}'\n",
            "activation",
        ),
    ] {
        std::fs::write(profiles.join(format!("{profile}.toml")), contents).unwrap();
        let error = validate_edit(&store, KEY_PROFILE, profile).unwrap_err();
        assert!(error.contains("Current configuration"), "{error}");
        assert!(error.to_lowercase().contains(reason), "{error}");
        assert_eq!(store.get(KEY_PROFILE), None);
        assert!(configured_appearance(Some(&store)).is_ok());
        assert!(configured_navigation(Some(&store)).is_ok());
    }
}

#[test]
fn display_provenance_and_boolean_behaviour_do_not_parse_each_other() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    store.set_user_global(KEY_SHOW_HINTS, "false");
    store.set_session_override(KEY_SHOW_HINTS, "FALSE");
    let setting = declaration(KEY_SHOW_HINTS).unwrap();
    assert_eq!(
        setting.read(Some(&store)).source,
        ConfigLayer::SessionOverride.as_str()
    );
    assert_eq!(
        setting.row(Some(&store)).control,
        SettingControl::Toggle { on: true }
    );
    assert!(configured_show_hints(Some(&store)));
    assert!(validate(KEY_SHOW_HINTS, "FALSE").is_err());
    store.set_session_override(KEY_SHOW_HINTS, "false");
    assert!(!configured_show_hints(Some(&store)));
    assert_eq!(
        setting.row(Some(&store)).control,
        SettingControl::Toggle { on: false }
    );
    store.set_user_global(KEY_PROFILE, "true");
    assert_eq!(
        declaration(KEY_PROFILE).unwrap().row(Some(&store)).control,
        SettingControl::Text
    );
    store.set_user_global(KEY_PREVIEW_SPLIT, "NaN");
    assert_eq!(
        configured_preview_split(Some(&store)),
        configured_preview_split(None)
    );
    assert_eq!(declaration(KEY_MAX_RESULTS).unwrap().read(None).source, "unset");
    assert_eq!(declaration(KEY_PREVIEW_SPLIT).unwrap().read(None).source, "unset");
    assert_eq!(
        declaration(KEY_PREVIEW_SPLIT).unwrap().read(Some(&store)).source,
        "user-global"
    );
    assert_eq!(
        configured_hotkey(None),
        BUILT_IN_DEFAULTS
            .iter()
            .find(|(key, _)| *key == KEY_ACTIVATION_HOTKEY)
            .unwrap()
            .1
    );
}

#[test]
fn same_registered_plugin_declaration_validates_but_values_never_cross_search_boundary() {
    let fixture = Fixture::new();
    let mut store = fixture.store();
    let owned = {
        let schemas = vec![DiscoveredSchema {
            plugin: PluginId("modern.fixture".into()),
            package: fixture.0.clone(),
            section: ConfigurationSection {
                fields: vec![field("secret_name", true), field("visible", false)],
            },
        }];
        for schema in &schemas {
            assert!(store
                .register_plugin_schema(&schema.plugin, &schema.section)
                .is_empty());
        }
        store.set_session_override(
            "plugins.modern.fixture.settings.visible",
            "EFFECTIVE_VALUE_NOT_METADATA",
        );
        let search = SettingsSearch::new(7, &[], &schemas, "linux").unwrap();
        let matches = search.search("visible", 7).unwrap();
        let validator = matches[0].plugin_field.unwrap();
        assert!(std::ptr::eq(validator, &schemas[0].section.fields[1]));
        assert!(validator.validate("PUBLIC_DEFAULT_NOT_METADATA").is_ok());
        assert!(validator.validate("outside-allowed-set").is_err());
        for token in [
            "secret_name",
            "SECRET_DESCRIPTION",
            "SECRET_DEFAULT",
            "SECRET_ALLOWED",
            "PUBLIC_DEFAULT_NOT_METADATA",
            "PUBLIC_ALLOWED_NOT_METADATA",
            "EFFECTIVE_VALUE_NOT_METADATA",
        ] {
            assert!(search.search_owned(token, 7).unwrap().hits.is_empty(), "{token}");
        }
        assert_eq!(
            search.search_owned("", 8).unwrap_err(),
            SettingsSearchError::StaleSchema { current: 7 }
        );
        let owned = search.search_owned("", 7).unwrap();
        let debug = format!("{search:?} {matches:?} {owned:?}");
        for forbidden in [
            "secret_name",
            "SECRET_DESCRIPTION",
            "SECRET_DEFAULT",
            "SECRET_ALLOWED",
            "PUBLIC_DEFAULT_NOT_METADATA",
            "PUBLIC_ALLOWED_NOT_METADATA",
            "EFFECTIVE_VALUE_NOT_METADATA",
        ] {
            assert!(!debug.contains(forbidden));
        }
        owned
    }; // schema and borrowing index both dropped; response has no retained references.
    assert_eq!(owned.hits.len(), 1);
    assert_eq!(owned.hits[0].navigation_target, "modern.fixture");
}

fn response_retained_bytes(response: &crate::settings_search::SettingsSearchResults) -> usize {
    std::mem::size_of_val(response)
        + response.hits.capacity() * std::mem::size_of::<crate::settings_search::OwnedSettingMatch>()
        + response
            .hits
            .iter()
            .map(|hit| {
                hit.key.capacity()
                    + hit.label.capacity()
                    + hit.description.capacity()
                    + hit.category.capacity()
                    + hit.navigation_target.capacity()
                    + hit.control.capacity()
            })
            .sum::<usize>()
}

#[test]
fn owned_search_caps_hit_count_and_total_metadata_bytes() {
    let fields = (0..200)
        .map(|index| field(&format!("field{index}"), false))
        .collect();
    let mut schemas = vec![DiscoveredSchema {
        plugin: PluginId("native.bounds".into()),
        package: "fixture".into(),
        section: ConfigurationSection { fields },
    }];
    let response = SettingsSearch::new(1, &[], &schemas, "linux")
        .unwrap()
        .search_owned("", 1)
        .unwrap();
    assert_eq!(response.hits.len(), 128);
    assert!(response.truncated);
    assert!(response_retained_bytes(&response) <= 64 * 1024);
    schemas[0].section.fields.truncate(2);
    for field in &mut schemas[0].section.fields {
        field.description = "x".repeat(40_000);
    }
    let search = SettingsSearch::new(2, &[], &schemas, "linux").unwrap();
    let response = search.search_owned("", 2).unwrap();
    assert_eq!(response.hits.len(), 1);
    assert!(response.truncated);
    assert!(response_retained_bytes(&response) <= 64 * 1024);
    assert_eq!(
        search.search_owned(&"x".repeat(4097), 2).unwrap_err(),
        SettingsSearchError::QueryLimit
    );
    schemas[0].section.fields[0].description = "x".repeat(1024 * 1024);
    assert_eq!(
        SettingsSearch::new(3, &[], &schemas, "linux").unwrap_err(),
        SettingsSearchError::MetadataLimit
    );
}

#[test]
fn owned_budget_includes_vec_slots_and_string_capacities_not_only_text_lengths() {
    // Payload alone fits exactly, but the slot/header do not.
    let oversized = "x".repeat(64 * 1024 - 1);
    let mut definition = crate::settings_search::SettingDefinition {
        key: "k",
        label: "",
        category: "",
        navigation_target: "",
        control: "",
        description: &oversized,
        requires_restart: false,
        secret: false,
        platforms: &[],
    };
    let response = SettingsSearch::new(1, &[definition], &[], "linux")
        .unwrap()
        .search_owned("", 1)
        .unwrap();
    assert!(response.hits.is_empty());
    assert_eq!(response.hits.capacity(), 0);
    assert!(response.truncated);
    assert!(response_retained_bytes(&response) <= 64 * 1024);

    let overhead = std::mem::size_of::<crate::settings_search::SettingsSearchResults>()
        + std::mem::size_of::<crate::settings_search::OwnedSettingMatch>();
    let fitting = "x".repeat(64 * 1024 - overhead - 1);
    definition.description = &fitting;
    let response = SettingsSearch::new(2, &[definition], &[], "linux")
        .unwrap()
        .search_owned("", 2)
        .unwrap();
    assert_eq!(response.hits.len(), 1);
    assert!(!response.truncated);
    assert!(response_retained_bytes(&response) <= 64 * 1024);
}

#[test]
fn unsupported_and_secret_declarations_are_absent_even_for_empty_queries() {
    let mut secret = declaration(KEY_PROFILE).unwrap().search_definition();
    secret.key = "SECRET_LAUNCHER_KEY";
    secret.description = "SECRET_LAUNCHER_DESCRIPTION";
    secret.secret = true;
    let mut password = secret;
    password.secret = false;
    password.control = "PASSWORD";
    assert!(!format!("{secret:?} {password:?}").contains("SECRET_LAUNCHER"));
    let schemas = [DiscoveredSchema {
        plugin: PluginId("modern.platform".into()),
        package: "fixture".into(),
        section: ConfigurationSection {
            fields: vec![ConfigurationField {
                platforms: vec!["windows".into()],
                ..field("windows_only", false)
            }],
        },
    }];
    let search = SettingsSearch::new(1, &[secret, password], &schemas, "linux").unwrap();
    assert!(search.search_owned("", 1).unwrap().hits.is_empty());
    let search = SettingsSearch::new(1, &[secret, password], &schemas, "windows").unwrap();
    let hits = search.search_owned("", 1).unwrap().hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, "plugins.modern.platform.settings.windows_only");
}
