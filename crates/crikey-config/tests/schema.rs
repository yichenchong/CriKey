//! Plugin schemas: validation, rejection and secret redaction (spec 21.3).

mod support;

use crikey_config::{ConfigError, ConfigLayer};
use crikey_core::PluginId;
use crikey_plugin_model::{
    REDACTED, RULE_ALLOWED, RULE_MINIMUM, RULE_REQUIRED, RULE_TYPE, RULE_UNKNOWN_FIELD,
};
use support::{schema, Fixture};

const THEME: &str = "plugins.modern.example.settings.theme";
const LIMIT: &str = "plugins.modern.example.settings.result-limit";
const API_KEY: &str = "plugins.modern.example.settings.api-key";

fn example() -> PluginId {
    PluginId("modern.example".to_owned())
}

/// The rule name and field of the single violation `problems` reports.
fn only_violation(problems: &[ConfigError]) -> (&str, &str) {
    assert_eq!(
        problems.len(),
        1,
        "expected exactly one problem, got {problems:?}"
    );
    let ConfigError::Schema { violation, .. } = &problems[0] else {
        panic!("expected a schema violation, got {:?}", problems[0]);
    };
    (violation.field.as_str(), violation.rule)
}

#[test]
fn a_declared_default_becomes_the_plugin_defaults_layer() {
    let fixture = Fixture::new("schema-default");
    let mut store = fixture.load().expect("an empty tree loads");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema(
            "[[configuration.field]]\nname = \"theme\"\ndefault = \"dark\"\n\n\
             [[configuration.field]]\nname = \"result-limit\"\ntype = \"integer\"\ndefault = 20\n",
        ),
        "linux",
    );
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(store.get(THEME), Some("dark"));
    assert_eq!(store.layer_of(THEME), Some(ConfigLayer::PluginDefaults));
    assert_eq!(store.get(LIMIT), Some("20"));
    assert_eq!(
        store.plugin_values(&example()).get("result-limit"),
        Some(&"20".to_owned())
    );
}

#[test]
fn a_value_that_breaks_a_declared_rule_is_reported_and_replaced_by_the_default() {
    let fixture = Fixture::new("schema-reject");
    fixture.plugin_settings("modern.example", "[settings]\nresult-limit = 0\n");
    let mut store = fixture.load().expect("every fixture file is valid");

    let problems = store.register_plugin_schema_for(
        &example(),
        &schema(
            "[[configuration.field]]\nname = \"result-limit\"\ntype = \"integer\"\n\
             default = 20\nminimum = 1\n",
        ),
        "linux",
    );
    assert_eq!(only_violation(&problems), ("result-limit", RULE_MINIMUM));
    assert_eq!(
        store.get(LIMIT),
        Some("20"),
        "a rejected value must be replaced by the declared default, not delivered"
    );
    assert_eq!(store.layer_of(LIMIT), Some(ConfigLayer::PluginDefaults));
    assert!(
        problems[0].to_string().contains("result-limit"),
        "the message must name the field: {}",
        problems[0]
    );
    assert!(
        problems[0].to_string().contains(RULE_MINIMUM),
        "the message must name the rule: {}",
        problems[0]
    );
}

#[test]
fn a_value_of_the_wrong_type_names_the_type_rule() {
    let fixture = Fixture::new("schema-type");
    fixture.plugin_settings("modern.example", "[settings]\nresult-limit = \"many\"\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema("[[configuration.field]]\nname = \"result-limit\"\ntype = \"integer\"\n"),
        "linux",
    );
    assert_eq!(only_violation(&problems), ("result-limit", RULE_TYPE));
    assert_eq!(store.get(LIMIT), None, "no default, so the field is simply unset");
}

#[test]
fn a_value_outside_the_declared_set_names_the_allowed_rule() {
    let fixture = Fixture::new("schema-allowed");
    fixture.plugin_settings("modern.example", "[settings]\ntheme = \"solar\"\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema(
            "[[configuration.field]]\nname = \"theme\"\ndefault = \"dark\"\n\
             allowed = [\"dark\", \"light\"]\n",
        ),
        "linux",
    );
    assert_eq!(only_violation(&problems), ("theme", RULE_ALLOWED));
}

#[test]
fn a_required_field_with_no_value_anywhere_is_reported() {
    let fixture = Fixture::new("schema-required");
    let mut store = fixture.load().expect("an empty tree loads");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema("[[configuration.field]]\nname = \"api-key\"\nrequired = true\n"),
        "linux",
    );
    assert_eq!(only_violation(&problems), ("api-key", RULE_REQUIRED));
}

#[test]
fn a_required_field_the_user_supplied_is_not_reported() {
    let fixture = Fixture::new("schema-required-met");
    fixture.plugin_settings("modern.example", "[settings]\napi-key = \"token\"\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema("[[configuration.field]]\nname = \"api-key\"\nrequired = true\n"),
        "linux",
    );
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn a_setting_the_plugin_does_not_declare_is_reported_as_an_unknown_field() {
    let fixture = Fixture::new("schema-unknown");
    fixture.plugin_settings("modern.example", "[settings]\nthemee = \"dark\"\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema("[[configuration.field]]\nname = \"theme\"\ndefault = \"dark\"\n"),
        "linux",
    );
    assert_eq!(only_violation(&problems), ("themee", RULE_UNKNOWN_FIELD));
}

#[test]
fn every_problem_is_reported_rather_than_only_the_first() {
    let fixture = Fixture::new("schema-all-problems");
    fixture.plugin_settings(
        "modern.example",
        "[settings]\nresult-limit = 0\ntheme = \"solar\"\n",
    );
    let mut store = fixture.load().expect("every fixture file is valid");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema(
            "[[configuration.field]]\nname = \"result-limit\"\ntype = \"integer\"\nminimum = 1\n\n\
             [[configuration.field]]\nname = \"theme\"\nallowed = [\"dark\"]\n",
        ),
        "linux",
    );
    assert_eq!(
        problems.len(),
        2,
        "an operator with two bad settings must learn about both: {problems:?}"
    );
}

#[test]
fn invalid_values_in_adjacent_layers_are_both_removed() {
    let fixture = Fixture::new("schema-adjacent-invalid");
    fixture.user_global("[launcher]\nprofile = \"work\"\n");
    fixture.profile("work", "[plugins.modern.example.settings]\nresult-limit = 0\n");
    fixture.plugin_settings("modern.example", "[settings]\nresult-limit = -1\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema(
            "[[configuration.field]]\nname = \"result-limit\"\n\
             type = \"integer\"\nminimum = 1\n",
        ),
        "linux",
    );
    assert_eq!(
        problems.len(),
        2,
        "both invalid winners must be reported: {problems:?}"
    );
    assert!(!store.plugin_values(&example()).contains_key("result-limit"));
}
#[test]
fn a_platform_restricted_configured_value_is_not_delivered() {
    let fixture = Fixture::new("schema-platform-configured");
    fixture.plugin_settings("modern.example", "[settings]\nregistry-path = \"shared\"\n");
    let mut store = fixture.load().expect("an empty tree loads");
    // Restricted to a platform this host is not, chosen from the host rather
    // than written down: the delivery path filters on the real
    // `std::env::consts::OS`, so a hard-coded "windows" is a restriction to
    // *this* platform on a Windows runner and the value is rightly delivered.
    let elsewhere = if cfg!(windows) { "linux" } else { "windows" };
    let section = schema(&format!(
        "[[configuration.field]]\nname = \"registry-path\"\nplatforms = [\"{elsewhere}\"]\n"
    ));
    store.register_plugin_schema_for(&example(), &section, std::env::consts::OS);
    assert!(!store.plugin_values(&example()).contains_key("registry-path"),);
    assert!(store
        .configuration_snapshot()
        .values_for(&example())
        .expect("schema plugin is present")
        .get("registry-path")
        .is_none());
}

#[test]
fn a_field_restricted_to_another_platform_contributes_no_default_here() {
    let fixture = Fixture::new("schema-platform");
    let mut store = fixture.load().expect("an empty tree loads");
    let section = schema(
        "[[configuration.field]]\nname = \"registry-path\"\ntype = \"path\"\n\
         default = \"HKCU\\\\Software\"\nplatforms = [\"windows\"]\n",
    );
    let problems = store.register_plugin_schema_for(&example(), &section, "linux");
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(
        store.get("plugins.modern.example.settings.registry-path"),
        None,
        "a Windows-only field must not carry its default onto a Linux host"
    );

    let mut windows = fixture.load().expect("an empty tree loads");
    windows.register_plugin_schema_for(&example(), &section, "windows");
    assert_eq!(
        windows.get("plugins.modern.example.settings.registry-path"),
        Some("HKCU\\Software")
    );
}

#[test]
fn a_required_field_restricted_to_another_platform_is_not_demanded_here() {
    let fixture = Fixture::new("schema-platform-required");
    let mut store = fixture.load().expect("an empty tree loads");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema(
            "[[configuration.field]]\nname = \"registry-path\"\nrequired = true\n\
             platforms = [\"windows\"]\n",
        ),
        "linux",
    );
    assert!(
        problems.is_empty(),
        "a field that does not apply here cannot be required here: {problems:?}"
    );
}

// ---------------------------------------------------------------------------
// Secrets (spec 21.3)
// ---------------------------------------------------------------------------

/// The schema used by every redaction test: one secret field, one ordinary one.
fn secret_schema() -> crikey_plugin_model::ConfigurationSection {
    schema(
        "[[configuration.field]]\nname = \"api-key\"\nsecret = true\n\n\
         [[configuration.field]]\nname = \"theme\"\ndefault = \"dark\"\n",
    )
}

/// The literal a test must never find rendered anywhere.
const TOKEN: &str = "sk-live-do-not-print-me";

#[test]
fn a_secret_value_is_never_rendered_by_the_stores_display_path() {
    let fixture = Fixture::new("secret-display");
    fixture.plugin_settings(
        "modern.example",
        &format!("[settings]\napi-key = \"{TOKEN}\"\ntheme = \"light\"\n"),
    );
    let mut store = fixture.load().expect("every fixture file is valid");
    store.register_plugin_schema_for(&example(), &secret_schema(), "linux");

    assert!(store.is_secret(API_KEY));
    assert!(!store.is_secret(THEME));
    assert_eq!(
        store.display_value(API_KEY),
        Some(REDACTED),
        "the display path must redact a declared secret"
    );
    assert_eq!(
        store.display_value(THEME),
        Some("light"),
        "an ordinary field must still be readable"
    );
}

#[test]
fn no_key_in_the_store_renders_a_secret_value_through_the_display_path() {
    // The guarantee is about the whole store, not one key: a redaction that only
    // worked for the key a test happened to name would be no protection at all.
    let fixture = Fixture::new("secret-sweep");
    fixture.user_global("[launcher]\nmax-results = 10\n");
    fixture.plugin_settings(
        "modern.example",
        &format!("[settings]\napi-key = \"{TOKEN}\"\ntheme = \"light\"\n"),
    );
    let mut store = fixture.load().expect("every fixture file is valid");
    store.register_plugin_schema_for(&example(), &secret_schema(), "linux");

    for key in store.keys() {
        let rendered = store.display_value(key).unwrap_or_default();
        assert!(
            !rendered.contains(TOKEN),
            "`{key}` rendered the secret value as `{rendered}`"
        );
    }
}

#[test]
fn a_secret_value_still_reaches_the_plugin_that_declared_it() {
    // Redaction is about what humans and logs see. A secret the owning plugin
    // could not read would make the flag useless rather than protective.
    let fixture = Fixture::new("secret-delivered");
    fixture.plugin_settings("modern.example", &format!("[settings]\napi-key = \"{TOKEN}\"\n"));
    let mut store = fixture.load().expect("every fixture file is valid");
    store.register_plugin_schema_for(&example(), &secret_schema(), "linux");
    assert_eq!(
        store.plugin_values(&example()).get("api-key"),
        Some(&TOKEN.to_owned())
    );
}

#[test]
fn a_rejected_secret_value_is_never_quoted_in_the_diagnostic() {
    let fixture = Fixture::new("secret-violation");
    fixture.plugin_settings("modern.example", &format!("[settings]\napi-key = \"{TOKEN}\"\n"));
    let mut store = fixture.load().expect("every fixture file is valid");
    let problems = store.register_plugin_schema_for(
        &example(),
        &schema(
            "[[configuration.field]]\nname = \"api-key\"\nsecret = true\n\
             allowed = [\"expected\"]\n",
        ),
        "linux",
    );
    let (field, rule) = only_violation(&problems);
    assert_eq!((field, rule), ("api-key", RULE_ALLOWED));
    let message = problems[0].to_string();
    assert!(
        !message.contains(TOKEN),
        "a secret leaked into a diagnostic: {message}"
    );
    assert!(message.contains(REDACTED), "{message}");
}

#[test]
fn a_key_whose_plugin_registered_no_schema_is_not_treated_as_secret() {
    let fixture = Fixture::new("secret-unknown-plugin");
    fixture.plugin_settings("modern.other", "[settings]\ntheme = \"dark\"\n");
    let store = fixture.load().expect("every fixture file is valid");
    assert!(!store.is_secret("plugins.modern.other.settings.theme"));
    assert_eq!(
        store.display_value("plugins.modern.other.settings.theme"),
        Some("dark"),
        "inventing secrecy for unknown keys would redact the whole store"
    );
}
