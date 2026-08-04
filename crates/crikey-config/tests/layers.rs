//! The seven configuration layers and their precedence (spec 21.2).
//!
//! Precedence is asserted by *observing which layer won*, never by asserting that
//! a value happens to be right. Each source writes a value that names its own
//! layer, so a failure reports the layer that incorrectly won rather than an
//! opaque string mismatch.

mod support;

use crikey_config::{ConfigLayer, KEY_COALESCE_MS, KEY_PROFILE};
use crikey_core::PluginId;
use support::{schema, Fixture};

/// The key every plugin-namespace layer competes for.
const THEME: &str = "plugins.modern.example.settings.theme";

fn example() -> PluginId {
    PluginId("modern.example".to_owned())
}

/// A schema whose declared default is the [`ConfigLayer::PluginDefaults`] value.
fn theme_schema() -> crikey_plugin_model::ConfigurationSection {
    schema("[[configuration.field]]\nname = \"theme\"\ndefault = \"plugin-defaults\"\n")
}

/// Writes every source that can supply `THEME` except the plugin default (which
/// is registered in code) and the session override (which is set in memory).
fn write_all_file_layers(fixture: &Fixture) {
    fixture.policy("[plugins.modern.example.settings]\ntheme = \"administrator-policy\"\n");
    fixture.user_global(
        "[launcher]\nprofile = \"work\"\n\n\
         [plugins.modern.example.settings]\ntheme = \"user-global\"\n",
    );
    fixture.profile("work", "[plugins.modern.example.settings]\ntheme = \"profile\"\n");
    fixture.plugin_settings("modern.example", "[settings]\ntheme = \"user-plugin\"\n");
}

#[test]
fn each_of_the_seven_layers_wins_in_turn_as_the_layer_above_it_is_removed() {
    // One pass per layer, highest precedence first. Each step removes exactly the
    // winning source and re-loads, so the value that surfaces next is decided by
    // the store's precedence rule rather than by the order a test wrote things.
    let fixture = Fixture::new("seven-layers");
    write_all_file_layers(&fixture);

    // 7. Session overrides.
    let mut store = fixture.load().expect("every fixture file is valid");
    store.register_plugin_schema_for(&example(), &theme_schema(), "linux");
    store.set_session_override(THEME, "session-override");
    assert_eq!(store.layer_of(THEME), Some(ConfigLayer::SessionOverride));
    assert_eq!(store.get(THEME), Some("session-override"));

    // 6. User plugin settings — the per-plugin file.
    store.clear_session_override(THEME);
    assert_eq!(store.layer_of(THEME), Some(ConfigLayer::UserPlugin));
    assert_eq!(store.get(THEME), Some("user-plugin"));

    // 5. Plugin defaults — the manifest's declared default.
    fixture.remove_plugin_settings("modern.example");
    let mut store = fixture.load().expect("every fixture file is valid");
    store.register_plugin_schema_for(&example(), &theme_schema(), "linux");
    assert_eq!(store.layer_of(THEME), Some(ConfigLayer::PluginDefaults));
    assert_eq!(store.get(THEME), Some("plugin-defaults"));

    // 4. Profile settings. Reached by NOT registering the schema, which is the
    //    honest way to remove layer 5: a plugin that declares no default for a
    //    field contributes nothing to it.
    let store = fixture.load().expect("every fixture file is valid");
    assert_eq!(store.layer_of(THEME), Some(ConfigLayer::Profile));
    assert_eq!(store.get(THEME), Some("profile"));

    // 3. User-global settings.
    fixture.remove_profile("work");
    let store = fixture.load().expect("every fixture file is valid");
    assert_eq!(store.layer_of(THEME), Some(ConfigLayer::UserGlobal));
    assert_eq!(store.get(THEME), Some("user-global"));

    // 2. Administrator policy.
    fixture.remove_user_global();
    let store = fixture.load().expect("every fixture file is valid");
    assert_eq!(store.layer_of(THEME), Some(ConfigLayer::AdministratorPolicy));
    assert_eq!(store.get(THEME), Some("administrator-policy"));

    // 1. Built-in defaults. The plugin namespace has no compiled-in values, so
    //    the host's own key is what proves layer 1 exists and loses to layer 2.
    fixture.remove_policy();
    let store = fixture.load().expect("an absent policy file is not an error");
    assert_eq!(store.layer_of(THEME), None, "no layer supplies the key any more");
    assert_eq!(
        store.layer_of(KEY_COALESCE_MS),
        Some(ConfigLayer::BuiltInDefaults),
        "the host's own default is layer 1"
    );

    // ...and layer 2 beats layer 1 for that same key.
    fixture.policy("[launcher]\nconfiguration-coalesce-ms = 42\n");
    let store = fixture.load().expect("every fixture file is valid");
    assert_eq!(
        store.layer_of(KEY_COALESCE_MS),
        Some(ConfigLayer::AdministratorPolicy)
    );
    assert_eq!(store.get(KEY_COALESCE_MS), Some("42"));
}

#[test]
fn a_configuration_directory_with_no_files_at_all_loads_the_built_in_defaults() {
    let fixture = Fixture::new("empty-tree");
    let store = fixture
        .load()
        .expect("a machine with no configuration must start");
    for (key, value) in crikey_config::BUILT_IN_DEFAULTS {
        assert_eq!(store.get(key), Some(*value), "{key} lost its built-in default");
        assert_eq!(store.layer_of(key), Some(ConfigLayer::BuiltInDefaults));
    }
    assert_eq!(store.get("launcher.nothing-declares-this"), None);
    assert_eq!(store.layer_of("launcher.nothing-declares-this"), None);
}

#[test]
fn the_profile_layer_is_read_from_the_profile_the_lower_layers_select() {
    let fixture = Fixture::new("profile-selection");
    fixture.user_global("[launcher]\nprofile = \"work\"\n");
    fixture.profile("work", "[launcher]\nmax-results = 10\n");
    fixture.profile("home", "[launcher]\nmax-results = 99\n");

    let store = fixture.load().expect("every fixture file is valid");
    assert_eq!(store.get(KEY_PROFILE), Some("work"));
    assert_eq!(
        store.get("launcher.max-results"),
        Some("10"),
        "the unselected profile must not be read"
    );
    assert_eq!(store.layer_of("launcher.max-results"), Some(ConfigLayer::Profile));
}

#[test]
fn an_administrator_can_pin_the_profile_a_user_has_not_chosen() {
    let fixture = Fixture::new("policy-profile");
    fixture.policy("[launcher]\nprofile = \"managed\"\n");
    fixture.profile("managed", "[launcher]\nmax-results = 5\n");

    let store = fixture.load().expect("every fixture file is valid");
    assert_eq!(
        store.layer_of(KEY_PROFILE),
        Some(ConfigLayer::AdministratorPolicy)
    );
    assert_eq!(store.get("launcher.max-results"), Some("5"));
}

#[test]
fn naming_a_profile_that_does_not_exist_is_not_a_failure_to_start() {
    let fixture = Fixture::new("missing-profile");
    fixture.user_global("[launcher]\nprofile = \"typo\"\n");
    let store = fixture
        .load()
        .expect("a mistyped profile name must not stop the launcher");
    assert_eq!(store.layer_of("launcher.max-results"), None);
}

#[test]
fn a_file_that_exists_but_cannot_be_parsed_is_reported_by_path() {
    let fixture = Fixture::new("bad-toml");
    fixture.user_global("this is not = = toml\n");
    let error = fixture
        .load()
        .expect_err("silently ignoring a user's settings is worse than refusing to start");
    let crikey_config::ConfigError::Parse { path, .. } = error else {
        panic!("expected a parse error naming the file, got {error}");
    };
    assert_eq!(path, fixture.config_dir().join("config.toml"));
}

#[test]
fn every_per_plugin_file_is_scoped_to_the_plugin_it_is_named_for() {
    let fixture = Fixture::new("per-plugin-scope");
    fixture.plugin_settings("modern.one", "[settings]\ntheme = \"one\"\n");
    fixture.plugin_settings("modern.two", "[settings]\ntheme = \"two\"\n");

    let store = fixture.load().expect("every fixture file is valid");
    assert_eq!(
        store
            .plugin_values(&PluginId("modern.one".to_owned()))
            .get("theme"),
        Some(&"one".to_owned())
    );
    assert_eq!(
        store
            .plugin_values(&PluginId("modern.two".to_owned()))
            .get("theme"),
        Some(&"two".to_owned()),
        "one plugin's file must not reach another plugin"
    );
}

#[test]
fn a_plugin_receives_its_field_names_without_the_host_key_namespace() {
    let fixture = Fixture::new("plugin-values");
    fixture.plugin_settings(
        "modern.example",
        "[settings]\ntheme = \"dark\"\nresult-limit = 20\n",
    );
    let store = fixture.load().expect("every fixture file is valid");
    let values = store.plugin_values(&example());
    assert_eq!(values.keys().collect::<Vec<_>>(), ["result-limit", "theme"]);
    assert_eq!(values.get("theme"), Some(&"dark".to_owned()));
    assert_eq!(values.get("result-limit"), Some(&"20".to_owned()));
}

#[test]
fn a_session_override_is_never_written_to_disk() {
    let fixture = Fixture::new("session-not-persisted");
    fixture.user_global("[launcher]\nmax-results = 10\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    store.set_session_override("launcher.max-results", "1");
    store.save().expect("the user file can be written");

    let reloaded = fixture.load().expect("the saved file is valid");
    assert_eq!(
        reloaded.get("launcher.max-results"),
        Some("10"),
        "a session override that survived the session would not be one"
    );
}
