//! Declaring remote catalog sources through the real store (spec 2.2; ADR-0016).
//!
//! The declaration *rules* are unit-tested beside the parser. What is defended
//! here is that the rules apply to a real layered store: a `[catalog.remote.*]`
//! table written to a file becomes a source, layering decides which value wins,
//! and a tree with no such table declares nothing.

mod support;

use crikey_config::{remote_catalog_sources, ConfigLayer, DEFAULT_REMOTE_INTERVAL_MS};
use support::Fixture;

#[test]
fn a_configuration_tree_with_no_catalog_table_declares_no_source() {
    let fixture = Fixture::new("remote-none");
    fixture.user_global("[launcher]\nprofile = \"work\"\n");
    let store = fixture.load().expect("the tree loads");

    assert!(
        remote_catalog_sources(&store)
            .expect("an absent table is not an error")
            .is_empty(),
        "remote indexing is off until an operator turns it on"
    );
}

#[test]
fn a_nested_toml_table_becomes_one_complete_source() {
    let fixture = Fixture::new("remote-one");
    fixture.user_global(
        "[catalog.remote.team]\n\
         url = \"https://index.example.com/team/index.txt\"\n\
         interval-ms = 900000\n\
         max-bytes = 1048576\n\
         require-signature = true\n\
         signing-key = \"team-index\"\n",
    );
    let store = fixture.load().expect("the tree loads");

    let source = remote_catalog_sources(&store)
        .expect("the declaration is complete")
        .remove(0);
    assert_eq!(source.name, "team");
    assert_eq!(source.url, "https://index.example.com/team/index.txt");
    assert_eq!(source.interval_ms, 900_000);
    assert_eq!(source.max_bytes, 1_048_576);
    assert!(source.require_signature);
    assert_eq!(source.signing_key.as_deref(), Some("team-index"));
}

#[test]
fn two_tables_declare_two_sources() {
    let fixture = Fixture::new("remote-two");
    fixture.user_global(
        "[catalog.remote.team]\nurl = \"https://a.example.com/i.txt\"\n\n\
         [catalog.remote.archive]\nurl = \"file:///srv/archive/i.txt\"\n",
    );
    let store = fixture.load().expect("the tree loads");

    let names: Vec<String> = remote_catalog_sources(&store)
        .expect("both declarations are complete")
        .into_iter()
        .map(|source| source.name)
        .collect();
    assert_eq!(names, vec!["archive".to_owned(), "team".to_owned()]);
}

#[test]
fn an_administrator_policy_pins_a_source_a_user_cannot_redirect() {
    let fixture = Fixture::new("remote-policy");
    fixture.policy("[catalog.remote.team]\nurl = \"https://pinned.example.com/i.txt\"\n");
    fixture.user_global("[catalog.remote.team]\nurl = \"https://elsewhere.example.com/i.txt\"\n");
    let store = fixture.load().expect("the tree loads");

    assert_eq!(
        store.layer_of("catalog.remote.team.url"),
        Some(ConfigLayer::AdministratorPolicy),
        "a remote source obeys the same precedence as every other key"
    );
    let source = remote_catalog_sources(&store)
        .expect("the declaration is complete")
        .remove(0);
    assert_eq!(source.url, "https://pinned.example.com/i.txt");
    assert_eq!(
        source.interval_ms, DEFAULT_REMOTE_INTERVAL_MS,
        "an unstated interval is the documented default, not zero"
    );
}

#[test]
fn a_malformed_declaration_is_reported_rather_than_ignored() {
    let fixture = Fixture::new("remote-malformed");
    fixture.user_global("[catalog.remote.team]\ninterval-ms = 1000\n");
    let store = fixture.load().expect("the tree is valid TOML");

    let error = remote_catalog_sources(&store).expect_err("a source with no url names nowhere");
    assert!(
        error.to_string().contains("catalog.remote.team.url"),
        "the message names the key the operator has to add: {error}"
    );
}
