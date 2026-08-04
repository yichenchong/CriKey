//! `[configuration]` as an author writes it in `crikey.toml` (spec 21.3).
//!
//! The unit tests beside [`crikey_plugin_model::configuration`] cover the
//! validation rules against constructed fields. These cover the part only a real
//! parse can prove: that the array-of-tables spelling reaches those fields, that
//! omitted keys take the documented defaults, that an unknown key is refused
//! rather than silently ignored, and that a manifest declaring a broken schema
//! fails to parse at all instead of loading and misbehaving later.

use crikey_plugin_model::{ConfigurationKind, Manifest, ManifestError, RULE_DEFAULT, RULE_DUPLICATE_FIELD};

/// The smallest manifest the model accepts, so each test below adds only the
/// `[configuration]` text it is about.
fn manifest_with(configuration: &str) -> String {
    format!(
        "manifest-version = 1\n\
         \n\
         [plugin]\n\
         id = \"example\"\n\
         name = \"Example\"\n\
         version = \"1.0.0\"\n\
         runtime = \"python\"\n\
         entrypoint = {{ any = \"example:Plugin\" }}\n\
         \n\
         {configuration}"
    )
}

#[test]
fn a_manifest_declaring_no_configuration_has_an_empty_schema() {
    let manifest = Manifest::parse(&manifest_with("")).expect("the section is optional");
    assert!(manifest.configuration.is_empty());
}

#[test]
fn every_declared_attribute_of_a_field_survives_the_parse() {
    let manifest = Manifest::parse(&manifest_with(
        "[[configuration.field]]\n\
         name = \"api-key\"\n\
         type = \"string\"\n\
         description = \"Token for the remote search API.\"\n\
         secret = true\n\
         requires-restart = true\n\
         required = true\n\
         platforms = [\"windows\", \"macos\"]\n\
         max-length = 64\n\
         \n\
         [[configuration.field]]\n\
         name = \"result-limit\"\n\
         type = \"integer\"\n\
         default = 20\n\
         minimum = 1\n\
         maximum = 200\n\
         \n\
         [[configuration.field]]\n\
         name = \"theme\"\n\
         default = \"dark\"\n\
         allowed = [\"dark\", \"light\"]\n",
    ))
    .expect("a fully populated section parses");

    let fields = &manifest.configuration.fields;
    assert_eq!(fields.len(), 3, "declaration order is preserved");

    let secret = manifest.configuration.field("api-key").expect("declared");
    assert_eq!(secret.kind, ConfigurationKind::String);
    assert!(secret.secret);
    assert!(secret.requires_restart);
    assert!(secret.required);
    assert_eq!(secret.platforms, ["windows", "macos"]);
    assert_eq!(secret.max_length, Some(64));
    assert!(secret.description.contains("remote search API"));
    assert!(!secret.applies_to("linux"), "a platform restriction is in force");

    let limit = manifest.configuration.field("result-limit").expect("declared");
    assert_eq!(limit.kind, ConfigurationKind::Integer);
    assert_eq!(
        limit.default_text().expect("a scalar default"),
        Some("20".to_owned())
    );
    assert_eq!(limit.minimum, Some(1));
    assert_eq!(limit.maximum, Some(200));

    let theme = manifest.configuration.field("theme").expect("declared");
    assert_eq!(
        theme.kind,
        ConfigurationKind::String,
        "an omitted type defaults to string"
    );
    assert!(!theme.secret, "an omitted secret flag defaults to false");
    assert!(
        theme.platforms.is_empty(),
        "an omitted restriction means every platform"
    );
    assert_eq!(theme.allowed, ["dark", "light"]);
}

#[test]
fn a_misspelled_field_attribute_is_rejected_rather_than_ignored() {
    let error = Manifest::parse(&manifest_with(
        "[[configuration.field]]\n\
         name = \"theme\"\n\
         requires_restart = true\n",
    ))
    .expect_err("the manifest vocabulary is kebab-case");
    assert!(
        matches!(error, ManifestError::Parse(_)),
        "a snake_case key must not be silently dropped: {error}"
    );
}

#[test]
fn a_manifest_whose_default_breaks_its_own_declared_rule_does_not_parse() {
    let error = Manifest::parse(&manifest_with(
        "[[configuration.field]]\n\
         name = \"result-limit\"\n\
         type = \"integer\"\n\
         default = 0\n\
         minimum = 1\n",
    ))
    .expect_err("a plugin broken on a machine with no settings must not load");
    let ManifestError::InvalidConfiguration(violation) = error else {
        panic!("expected a configuration violation, got {error}");
    };
    assert_eq!(violation.field, "result-limit");
    assert_eq!(violation.rule, RULE_DEFAULT);
}

#[test]
fn a_manifest_declaring_one_field_twice_does_not_parse() {
    let error = Manifest::parse(&manifest_with(
        "[[configuration.field]]\n\
         name = \"theme\"\n\
         \n\
         [[configuration.field]]\n\
         name = \"theme\"\n",
    ))
    .expect_err("a shadowed field has no defined winner");
    let ManifestError::InvalidConfiguration(violation) = error else {
        panic!("expected a configuration violation, got {error}");
    };
    assert_eq!(violation.rule, RULE_DUPLICATE_FIELD);
}

#[test]
fn a_type_the_model_does_not_define_is_rejected() {
    let error = Manifest::parse(&manifest_with(
        "[[configuration.field]]\n\
         name = \"port\"\n\
         type = \"uint16\"\n",
    ))
    .expect_err("an undefined type has no validation rule");
    assert!(matches!(error, ManifestError::Parse(_)), "{error}");
}
