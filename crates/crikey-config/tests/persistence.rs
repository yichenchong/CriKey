//! What `save` writes, and what it must leave alone (spec 21.2).

mod support;

use crikey_config::{ConfigLayer, KEY_ACTIVATION_HOTKEY};
use crikey_core::PluginId;
use crikey_plugin_model::SchedulingProfile;
use support::Fixture;

fn example() -> PluginId {
    PluginId("modern.example".to_owned())
}

#[test]
fn an_unknown_plugin_is_enabled_and_has_no_pinned_scheduling_profile() {
    let fixture = Fixture::new("defaults-enabled");
    let store = fixture.load().expect("an empty tree loads");
    assert!(store.plugin_enabled(&example()));
    assert_eq!(store.scheduling_profile(&example()), None);
    assert!(store.disabled_plugins().is_empty());
}

#[test]
fn disabling_a_plugin_survives_a_save_and_reload() {
    let fixture = Fixture::new("disable-roundtrip");
    let mut store = fixture.load().expect("an empty tree loads");
    store.set_plugin_enabled(&example(), false);
    store.save().expect("the user file can be written");

    let reloaded = fixture.load().expect("the saved file is valid");
    assert!(!reloaded.plugin_enabled(&example()));
    assert_eq!(
        reloaded.layer_of("plugins.modern.example.enabled"),
        Some(ConfigLayer::UserGlobal),
        "save writes the user-global layer"
    );
    assert_eq!(
        reloaded.disabled_plugins(),
        std::collections::BTreeSet::from(["modern.example".to_owned()])
    );
}

#[test]
fn re_enabling_a_plugin_survives_a_save_and_reload() {
    let fixture = Fixture::new("enable-roundtrip");
    let mut store = fixture.load().expect("an empty tree loads");
    store.set_plugin_enabled(&example(), false);
    store.save().expect("write");
    let mut store = fixture.load().expect("read back");
    store.set_plugin_enabled(&example(), true);
    store.save().expect("write");

    let reloaded = fixture.load().expect("read back");
    assert!(reloaded.plugin_enabled(&example()));
    assert!(reloaded.disabled_plugins().is_empty());
}

#[test]
fn a_pinned_scheduling_profile_survives_a_save_and_reload_and_can_be_removed() {
    let fixture = Fixture::new("scheduling-roundtrip");
    let mut store = fixture.load().expect("an empty tree loads");
    store.set_scheduling_profile(&example(), Some(SchedulingProfile::Modern));
    store.save().expect("write");

    let mut store = fixture.load().expect("read back");
    assert_eq!(
        store.scheduling_profile(&example()),
        Some(SchedulingProfile::Modern)
    );

    store.set_scheduling_profile(&example(), None);
    store.save().expect("write");
    let reloaded = fixture.load().expect("read back");
    assert_eq!(reloaded.scheduling_profile(&example()), None);
}

#[test]
fn an_unrecognised_scheduling_profile_leaves_the_manifests_choice_in_force() {
    let fixture = Fixture::new("scheduling-typo");
    fixture.user_global("[plugins.modern.example]\nscheduling-profile = \"modrn\"\n");
    let store = fixture.load().expect("every fixture file is valid");
    assert_eq!(
        store.scheduling_profile(&example()),
        None,
        "a typo must not take a plugin out of the query path"
    );
}

#[test]
fn only_the_exact_text_false_disables_a_plugin() {
    let fixture = Fixture::new("enabled-spelling");
    fixture.user_global(
        "[plugins.modern.yes]\nenabled = true\n\n\
         [plugins.modern.no]\nenabled = false\n\n\
         [plugins.modern.typo]\nenabled = \"nope\"\n",
    );
    let store = fixture.load().expect("every fixture file is valid");
    assert!(store.plugin_enabled(&PluginId("modern.yes".to_owned())));
    assert!(!store.plugin_enabled(&PluginId("modern.no".to_owned())));
    assert!(
        store.plugin_enabled(&PluginId("modern.typo".to_owned())),
        "a typo must not silently disable a plugin"
    );
    assert_eq!(
        store.disabled_plugins(),
        std::collections::BTreeSet::from(["modern.no".to_owned()])
    );
}

#[test]
fn saving_does_not_absorb_the_administrator_policy_into_the_users_file() {
    let fixture = Fixture::new("policy-not-absorbed");
    fixture.policy("[launcher]\nmax-results = 5\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    store.set_plugin_enabled(&example(), false);
    store.save().expect("write");

    let written = std::fs::read_to_string(fixture.config_dir().join("config.toml")).expect("read");
    assert!(
        !written.contains("max-results"),
        "a policy the administrator owns must not become a copy the user owns: {written}"
    );

    fixture.remove_policy();
    let reloaded = fixture.load().expect("read back");
    assert_eq!(
        reloaded.get("launcher.max-results"),
        None,
        "the policy value was never persisted, so removing the policy removes it"
    );
}

#[test]
fn saving_does_not_absorb_a_per_plugin_file_into_the_users_global_file() {
    let fixture = Fixture::new("plugin-file-not-absorbed");
    fixture.plugin_settings("modern.example", "[settings]\ntheme = \"dark\"\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    store.set_plugin_enabled(&example(), false);
    store.save().expect("write");

    let written = std::fs::read_to_string(fixture.config_dir().join("config.toml")).expect("read");
    assert!(!written.contains("theme"), "{written}");
    assert!(written.contains("enabled"), "{written}");
}

#[test]
fn a_save_preserves_every_value_the_user_file_already_held() {
    let fixture = Fixture::new("save-preserves");
    fixture.user_global(
        "[launcher]\nprofile = \"work\"\nmax-results = 10\n\n\
         [plugins.modern.other]\nenabled = false\n",
    );
    let mut store = fixture.load().expect("every fixture file is valid");
    store.set_plugin_enabled(&example(), false);
    store.save().expect("write");

    let reloaded = fixture.load().expect("read back");
    assert_eq!(reloaded.get("launcher.profile"), Some("work"));
    assert_eq!(reloaded.get("launcher.max-results"), Some("10"));
    assert!(!reloaded.plugin_enabled(&PluginId("modern.other".to_owned())));
    assert!(!reloaded.plugin_enabled(&example()));
}

#[test]
fn saving_creates_the_configuration_directory_when_it_does_not_exist_yet() {
    // A first run must be able to persist a choice the user just made, and on a
    // first run nothing has created `config_dir()` yet.
    let fixture = Fixture::new("save-creates-dir");
    let mut store = fixture.load().expect("an empty tree loads");
    std::fs::remove_dir_all(fixture.config_dir()).expect("remove the directory entirely");
    store.set_plugin_enabled(&example(), false);
    store.save().expect("save recreates the directory");
    assert!(fixture.config_dir().join("config.toml").exists());
}

#[test]
fn the_activation_hotkey_has_a_default_a_fresh_machine_can_read() {
    // The launcher binds this chord at startup. On a machine with no
    // configuration at all the user still has to be able to find out what it
    // is, so the default is a value the store supplies rather than a constant
    // buried in the host.
    let fixture = Fixture::new("hotkey-default");
    let store = fixture.load().expect("an empty tree loads");
    assert_eq!(store.get(KEY_ACTIVATION_HOTKEY), Some("Ctrl+Alt+Space"));
    assert_eq!(
        store.layer_of(KEY_ACTIVATION_HOTKEY),
        Some(ConfigLayer::BuiltInDefaults)
    );
}

#[test]
fn a_written_activation_hotkey_survives_a_save_and_reload_in_the_user_global_layer() {
    let fixture = Fixture::new("hotkey-roundtrip");
    let mut store = fixture.load().expect("an empty tree loads");
    store.set_user_global(KEY_ACTIVATION_HOTKEY, "Ctrl+Shift+P");
    store.save().expect("the user file can be written");

    let reloaded = fixture.load().expect("the saved file is valid");
    assert_eq!(reloaded.get(KEY_ACTIVATION_HOTKEY), Some("Ctrl+Shift+P"));
    assert_eq!(
        reloaded.layer_of(KEY_ACTIVATION_HOTKEY),
        Some(ConfigLayer::UserGlobal)
    );
}

#[test]
fn a_user_global_write_does_not_override_the_layers_above_it() {
    // The settings surface writes one layer, not the winning value. The
    // selected profile outranks the user's global file, so a panel that
    // reported its own write as effective would tell the user their launcher
    // now answers to a chord it does not answer to.
    let fixture = Fixture::new("user-global-outranked");
    fixture.user_global("[launcher]\nprofile = \"work\"\n");
    fixture.profile("work", "[launcher]\nactivation-hotkey = \"Ctrl+Alt+K\"\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    store.set_user_global(KEY_ACTIVATION_HOTKEY, "Ctrl+Shift+P");
    store.save().expect("write");

    let reloaded = fixture.load().expect("read back");
    assert_eq!(reloaded.get(KEY_ACTIVATION_HOTKEY), Some("Ctrl+Alt+K"));
    assert_eq!(
        reloaded.layer_of(KEY_ACTIVATION_HOTKEY),
        Some(ConfigLayer::Profile),
        "the user's write is persisted but does not win"
    );
}
