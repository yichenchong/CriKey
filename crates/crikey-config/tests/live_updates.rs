//! Live configuration updates end to end (spec 21.4).
//!
//! The publisher's own unit tests cover the timing rules against constructed
//! snapshots. These cover the part only the real store can prove: that what gets
//! published is the *complete* state read back off disk, that a burst of file
//! edits reaches a plugin exactly once with the final content, and that an
//! explicit apply does not wait.

mod support;

use std::time::{Duration, Instant};

use crikey_config::{ConfigStore, ConfigurationPublisher, ConfigurationSnapshot};
use crikey_core::PluginId;
use support::{schema, Fixture};

fn one() -> PluginId {
    PluginId("modern.one".to_owned())
}

fn two() -> PluginId {
    PluginId("modern.two".to_owned())
}

/// Reloads the store the way the launcher does, schemas and all.
///
/// The launcher re-registers every schema after a reload because the plugin
/// defaults layer lives in memory: a reload that dropped it would hand plugins a
/// state with their own defaults missing, which is precisely an incomplete state.
fn reload(fixture: &Fixture) -> ConfigStore {
    let mut store = fixture.load().expect("every fixture file is valid");
    let section = schema(
        "[[configuration.field]]\nname = \"theme\"\ndefault = \"dark\"\n\n\
         [[configuration.field]]\nname = \"result-limit\"\ntype = \"integer\"\ndefault = 20\n",
    );
    for plugin in [one(), two()] {
        let problems = store.register_plugin_schema_for(&plugin, &section, "linux");
        assert!(problems.is_empty(), "{problems:?}");
    }
    store
}

fn publisher() -> ConfigurationPublisher {
    ConfigurationPublisher::new(Duration::from_millis(150), Duration::from_millis(1_000))
}

#[test]
fn the_published_state_is_the_complete_state_of_every_plugin() {
    let fixture = Fixture::new("live-complete");
    fixture.plugin_settings("modern.one", "[settings]\ntheme = \"light\"\n");
    let store = reload(&fixture);

    let mut publisher = publisher();
    let start = Instant::now();
    publisher.observe(store.configuration_snapshot(), start);
    let published = publisher
        .poll(start + Duration::from_millis(150))
        .expect("the state is due");

    // Complete means: both plugins, and each with every field it has — the one
    // the user set AND the one only the manifest declares. A publication missing
    // the untouched field would leave the plugin guessing whether the field was
    // removed or simply not mentioned.
    assert_eq!(published.plugins().len(), 2, "{published:?}");
    let first = published.values_for(&one()).expect("plugin one is present");
    assert_eq!(first.get("theme"), Some(&"light".to_owned()));
    assert_eq!(first.get("result-limit"), Some(&"20".to_owned()));
    let second = published.values_for(&two()).expect("plugin two is present");
    assert_eq!(second.get("theme"), Some(&"dark".to_owned()));
    assert_eq!(second.get("result-limit"), Some(&"20".to_owned()));
}

#[test]
fn a_burst_of_file_edits_reaches_a_plugin_once_carrying_only_the_final_state() {
    let fixture = Fixture::new("live-burst");
    let mut publisher = publisher();
    let start = Instant::now();

    // Four writes 10 ms apart, the way an editor saves on every keystroke. The
    // launcher observes each one; none may be published.
    let mut delivered: Vec<ConfigurationSnapshot> = Vec::new();
    for (offset, theme) in [(0, "l"), (10, "li"), (20, "ligh"), (30, "light")] {
        fixture.plugin_settings("modern.one", &format!("[settings]\ntheme = \"{theme}\"\n"));
        let now = start + Duration::from_millis(offset);
        publisher.observe(reload(&fixture).configuration_snapshot(), now);
        if let Some(state) = publisher.poll(now) {
            delivered.push(state);
        }
    }
    assert!(
        delivered.is_empty(),
        "an intermediate edit reached a plugin: {delivered:?}"
    );
    assert_eq!(publisher.coalesced(), 3, "three intermediate states were dropped");

    let published = publisher
        .poll(start + Duration::from_millis(180))
        .expect("150 ms after the last write the state is due");
    assert_eq!(
        published
            .values_for(&one())
            .and_then(|values| values.get("theme")),
        Some(&"light".to_owned()),
        "only the final content is published"
    );
    assert!(
        !publisher.has_pending(),
        "the burst closed; nothing is left to publish"
    );
    assert_eq!(
        publisher.poll(start + Duration::from_secs(10)),
        None,
        "a settled burst must not publish again"
    );
}

#[test]
fn an_explicit_apply_publishes_the_state_without_waiting_for_the_burst_to_settle() {
    let fixture = Fixture::new("live-apply");
    fixture.plugin_settings("modern.one", "[settings]\ntheme = \"light\"\n");
    let mut publisher = publisher();
    let start = Instant::now();
    publisher.observe(reload(&fixture).configuration_snapshot(), start);

    assert_eq!(publisher.poll(start), None, "the quiet time has not elapsed");
    let published = publisher.flush().expect("an explicit apply bypasses the delay");
    assert_eq!(
        published
            .values_for(&one())
            .and_then(|values| values.get("theme")),
        Some(&"light".to_owned())
    );
}

#[test]
fn touching_a_file_without_changing_it_publishes_nothing() {
    let fixture = Fixture::new("live-no-op");
    fixture.plugin_settings("modern.one", "[settings]\ntheme = \"light\"\n");
    let mut publisher = publisher();
    let start = Instant::now();
    publisher.observe(reload(&fixture).configuration_snapshot(), start);
    publisher
        .poll(start + Duration::from_millis(150))
        .expect("the first state publishes");

    // Rewrite byte-for-byte identical content: the source watch will notice the
    // timestamp, the launcher will reload, and the publisher must still say
    // nothing rather than wake every plugin over a no-op.
    fixture.plugin_settings("modern.one", "[settings]\ntheme = \"light\"\n");
    publisher.observe(
        reload(&fixture).configuration_snapshot(),
        start + Duration::from_secs(1),
    );
    assert!(!publisher.has_pending());
    assert_eq!(publisher.poll(start + Duration::from_secs(2)), None);
}

#[test]
fn a_change_to_a_file_is_noticed_by_the_stores_own_source_watch() {
    let fixture = Fixture::new("live-watch");
    let store = fixture.load().expect("an empty tree loads");
    let watch = store.source_watch();
    assert!(!watch.changed(), "nothing has been written yet");
    assert!(
        watch.paths().any(|path| path.ends_with("config.toml")),
        "the user's own file must be watched even before it exists"
    );

    fixture.user_global("[launcher]\nmax-results = 10\n");
    assert!(watch.changed(), "the first config.toml a user writes is a change");
}

#[test]
fn a_plugin_that_loses_every_setting_still_appears_in_the_published_state() {
    // A plugin dropped from the snapshot would keep applying whatever it was last
    // sent, so "complete" has to include plugins with nothing set.
    let fixture = Fixture::new("live-emptied");
    fixture.plugin_settings("modern.one", "[settings]\ntheme = \"light\"\n");
    let mut store = fixture.load().expect("every fixture file is valid");
    store.register_plugin_schema_for(
        &one(),
        &schema("[[configuration.field]]\nname = \"theme\"\n"),
        "linux",
    );
    assert_eq!(
        store
            .configuration_snapshot()
            .values_for(&one())
            .map(BTreeMapLen::len),
        Some(1)
    );

    fixture.remove_plugin_settings("modern.one");
    let mut store = fixture.load().expect("every fixture file is valid");
    store.register_plugin_schema_for(
        &one(),
        &schema("[[configuration.field]]\nname = \"theme\"\n"),
        "linux",
    );
    let snapshot = store.configuration_snapshot();
    assert_eq!(
        snapshot.values_for(&one()).map(BTreeMapLen::len),
        Some(0),
        "the plugin is still named, with an empty map"
    );
}

/// Only so the assertions above can read a map's length without spelling out its
/// full type twice.
trait BTreeMapLen {
    fn len(&self) -> usize;
}

impl BTreeMapLen for std::collections::BTreeMap<String, String> {
    fn len(&self) -> usize {
        std::collections::BTreeMap::len(self)
    }
}
