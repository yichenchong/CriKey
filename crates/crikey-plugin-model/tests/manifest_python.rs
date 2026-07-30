//! Modern-Python manifest fields (spec 15.2, 19; M4 contract §6).
//!
//! M4 adds an optional `[python]` section to `crikey.toml` carrying the two
//! inputs a modern plugin's managed environment is resolved from: the
//! `requires-python` gate and the declared `dependencies`. These tests pin the
//! parse shape of that section against the spec 15.2 example, the default an
//! *absent* section resolves to, the rejection of an unknown key inside it, and
//! that a parsed manifest round-trips through serialisation without losing the
//! python inputs. Nothing here spawns an interpreter; resolution and worker
//! behaviour live in the package-manager and python-host crates.
//!
//! Two properties drive the design:
//!
//! * The section is optional but not silently lossy. A plugin that omits
//!   `[python]` still parses and yields an empty-dependency, no-requirement
//!   default; a plugin that declares deps must retain them verbatim, in order.
//! * `deny_unknown_fields` is a typo guard. A misspelled key inside `[python]`
//!   would otherwise be dropped, resolving the plugin against dependencies its
//!   author never asked for, so it is rejected outright.

use crikey_plugin_model::{Manifest, ManifestError, PythonSection, Runtime};

/// The modern-python manifest printed in spec 15.2, byte for byte.
const SPEC_PYTHON_EXAMPLE: &str = include_str!("data/spec-python.crikey.toml");

/// A minimal python manifest whose extra `sections` text is appended verbatim.
/// Keeping the boilerplate in one place lets each test body show only the
/// python inputs it is actually pinning.
fn manifest_text(sections: &str) -> String {
    format!(
        "manifest-version = 1\n\
         \n\
         [plugin]\n\
         id = \"example.search\"\n\
         name = \"Example\"\n\
         version = \"1.0.0\"\n\
         runtime = \"python\"\n\
         entrypoint = \"example_search.plugin:Plugin\"\n\
         {sections}"
    )
}

fn parse(sections: &str) -> Manifest {
    Manifest::parse(&manifest_text(sections)).expect("manifest must parse")
}

// ---------------------------------------------------------------------------
// The specification example
// ---------------------------------------------------------------------------

/// Spec 15.2's example carries every M4 python input: the runtime, the
/// single-string entrypoint, the requires-python gate and two dependencies.
/// Each value must survive parsing attributed to the right field.
#[test]
fn spec_python_example_parses_every_field() {
    let manifest = Manifest::parse(SPEC_PYTHON_EXAMPLE).expect("spec example must parse");

    assert_eq!(manifest.plugin.runtime, Runtime::Python);

    // A single-string entrypoint is stored under the runtime-neutral "any" key.
    assert_eq!(
        manifest.plugin.entrypoint.get("any").map(String::as_str),
        Some("example_search.plugin:Plugin"),
        "single-string entrypoint is retrievable under the \"any\" key"
    );

    assert_eq!(
        manifest.python.requires_python.as_deref(),
        Some(">=3.12"),
        "requires-python is read from the kebab-case key"
    );
    assert_eq!(
        manifest.python.dependencies,
        vec!["httpx>=0.28,<1".to_string(), "pydantic>=2.9,<3".to_string(),],
        "dependencies are retained verbatim and in declared order"
    );
}

/// Dependency order is a resolution input, not a set: a resolver walks the list
/// in order, so a manifest that reorders its deps must parse to a different
/// vector rather than being normalised into the same one.
#[test]
fn dependency_order_is_preserved() {
    let manifest = parse(
        "\n[python]\n\
         dependencies = [\"pydantic>=2.9,<3\", \"httpx>=0.28,<1\"]\n",
    );
    assert_eq!(
        manifest.python.dependencies,
        vec!["pydantic>=2.9,<3".to_string(), "httpx>=0.28,<1".to_string(),]
    );
}

// ---------------------------------------------------------------------------
// Absent section
// ---------------------------------------------------------------------------

/// A manifest with no `[python]` table still parses. The section is optional,
/// so its absence resolves to the default: no requirement and no dependencies.
#[test]
fn a_manifest_without_a_python_section_yields_the_default() {
    let manifest = parse("");

    assert_eq!(manifest.python.requires_python, None);
    assert!(manifest.python.dependencies.is_empty());

    let default = PythonSection::default();
    assert_eq!(manifest.python.requires_python, default.requires_python);
    assert_eq!(manifest.python.dependencies, default.dependencies);
}

/// A `[python]` section may declare `requires-python` alone; an omitted
/// `dependencies` key is an empty list, not a parse failure. Absence of one
/// field must not force the other.
#[test]
fn requires_python_without_dependencies_parses() {
    let manifest = parse("\n[python]\nrequires-python = \">=3.14\"\n");
    assert_eq!(manifest.python.requires_python.as_deref(), Some(">=3.14"));
    assert!(manifest.python.dependencies.is_empty());
}

/// The mirror case: dependencies may be declared without a version gate, and
/// the absent `requires-python` stays `None` rather than defaulting to a
/// version the author never wrote.
#[test]
fn dependencies_without_requires_python_parses() {
    let manifest = parse("\n[python]\ndependencies = [\"httpx>=0.28,<1\"]\n");
    assert_eq!(manifest.python.requires_python, None);
    assert_eq!(manifest.python.dependencies, vec!["httpx>=0.28,<1".to_string()]);
}

// ---------------------------------------------------------------------------
// Unknown keys
// ---------------------------------------------------------------------------

/// `deny_unknown_fields` on the section is a typo guard: a misspelled key would
/// otherwise be dropped and the plugin resolved against inputs its author never
/// declared, so it is rejected as a parse error.
#[test]
fn an_unknown_key_inside_python_is_rejected() {
    let error = Manifest::parse(&manifest_text(
        "\n[python]\nrequires-python = \">=3.12\"\ndependancies = [\"httpx\"]\n",
    ))
    .expect_err("an unknown [python] key must be rejected");
    assert!(
        matches!(error, ManifestError::Parse(_)),
        "unknown-field rejection surfaces as a parse error, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

/// A parsed manifest must serialise back and re-parse to the same python
/// inputs. Tooling that rewrites a manifest (spec 26.3 dev flows) relies on the
/// python section surviving the trip, so a dropped or renamed field turns this
/// red.
#[test]
fn python_section_round_trips_through_serialisation() {
    let original = Manifest::parse(SPEC_PYTHON_EXAMPLE).expect("spec example must parse");

    let serialised = toml::to_string(&original).expect("manifest must serialise");
    let reparsed = Manifest::parse(&serialised).expect("serialised manifest must re-parse");

    assert_eq!(
        reparsed.plugin.runtime, original.plugin.runtime,
        "runtime survives the round trip"
    );
    assert_eq!(
        reparsed.python.requires_python, original.python.requires_python,
        "requires-python survives the round trip"
    );
    assert_eq!(
        reparsed.python.dependencies, original.python.dependencies,
        "dependencies survive the round trip"
    );
}
