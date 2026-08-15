//! User-defined query aliases as configuration (spec 21.2).
//!
//! Aliases are an *open* key space: the user names abbreviations the host could
//! not have declared a schema for. These tests pin that the open space still
//! obeys the ordinary layer precedence, and that a higher layer can retract an
//! alias a lower one defined.

mod support;

use support::Fixture;

#[test]
fn an_alias_table_is_read_as_alias_to_target_pairs() {
    let fixture = Fixture::new("aliases-basic");
    fixture.user_global("[aliases]\nss = \"Settings\"\nvsc = \"Visual Studio Code\"\n");

    let aliases = fixture.load().expect("configuration loads").aliases();

    assert_eq!(aliases.get("ss").map(String::as_str), Some("Settings"));
    assert_eq!(aliases.get("vsc").map(String::as_str), Some("Visual Studio Code"));
    assert_eq!(aliases.len(), 2, "no other keys are invented, got {aliases:?}");
}

/// The open key space is not exempt from the layer model: a profile overrides
/// the global file for the same alias, exactly as a declared setting would.
#[test]
fn a_higher_layer_overrides_an_alias_from_a_lower_one() {
    let fixture = Fixture::new("aliases-precedence");
    fixture.user_global("[launcher]\nprofile = \"work\"\n\n[aliases]\ned = \"Notepad\"\n");
    fixture.profile("work", "[aliases]\ned = \"Visual Studio Code\"\n");

    let aliases = fixture.load().expect("configuration loads").aliases();

    assert_eq!(
        aliases.get("ed").map(String::as_str),
        Some("Visual Studio Code"),
        "the profile layer outranks the user-global one"
    );
}

/// Layers merge by key, so without this there would be no spelling at all for
/// "not this one" - a profile could add aliases but never drop one.
#[test]
fn an_empty_target_retracts_an_alias_a_lower_layer_defined() {
    let fixture = Fixture::new("aliases-retraction");
    fixture.user_global("[launcher]\nprofile = \"work\"\n\n[aliases]\ned = \"Notepad\"\n");
    fixture.profile("work", "[aliases]\ned = \"\"\n");

    let aliases = fixture.load().expect("configuration loads").aliases();

    assert!(
        !aliases.contains_key("ed"),
        "the profile retracted the alias, got {aliases:?}"
    );
}

/// An administrator can ship a house alias without the user having written one.
#[test]
fn an_administrator_policy_can_define_an_alias() {
    let fixture = Fixture::new("aliases-policy");
    fixture.policy("[aliases]\nhelp = \"Support Portal\"\n");

    let aliases = fixture.load().expect("configuration loads").aliases();

    assert_eq!(aliases.get("help").map(String::as_str), Some("Support Portal"));
}

/// Aliases are optional; a configuration without the table is not an error and
/// does not synthesise entries.
#[test]
fn a_configuration_without_the_table_defines_no_aliases() {
    let fixture = Fixture::new("aliases-absent");
    fixture.user_global("[launcher]\nmax-results = \"50\"\n");

    assert!(fixture.load().expect("configuration loads").aliases().is_empty());
}
