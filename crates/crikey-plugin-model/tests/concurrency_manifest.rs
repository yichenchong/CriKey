//! Declared per-plugin concurrency budgets (spec 13.5).
//!
//! Spec 13.5 lets a modern manifest cap four independent kinds of simultaneous
//! work: suggestion requests, action requests, background tasks and
//! catalog-build tasks. These tests pin the *declaration* half of that
//! contract — what `[concurrency]` may contain, what silence means, and which
//! declarations are refused. Enforcement lives at the supervisor seam and is
//! pinned in `crikey-plugin-supervisor/tests/concurrency_budget.rs`.
//!
//! Two properties drive the design, mirroring `manifest_scheduling.rs`:
//!
//! * An omitted budget is not a zero declaration and is not unlimited. It is
//!   `None` at the declaration layer so the enforcement layer can apply its
//!   bounded host default. An explicit `= 0` is a deliberate "never
//!   permitted" declaration. Collapsing the two would either mute a plugin or
//!   uncap it, so both survive parsing as distinct `Option`s.
//! * A budget the host silently ignores is worse than no budget at all, so a
//!   misspelled key inside `[concurrency]` is a hard rejection rather than a
//!   field that quietly stays `None`.

use crikey_plugin_model::{ConcurrencySection, Manifest};

/// Builds a manifest around `sections`, which supplies everything after
/// `[plugin]`, so each test body shows only the concurrency inputs it pins.
fn manifest_text(sections: &str) -> String {
    format!(
        "manifest-version = 1\n\
         \n\
         [plugin]\n\
         id = \"dev.example.concurrency\"\n\
         name = \"Concurrency Fixture\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint = \"bin/plugin\"\n\
         {sections}"
    )
}

fn parse(sections: &str) -> Manifest {
    Manifest::parse(&manifest_text(sections)).expect("manifest must parse")
}

fn concurrency(sections: &str) -> ConcurrencySection {
    parse(sections).concurrency
}

/// All four budgets are separate knobs, so the fixture gives each one a
/// different value: a field-swap or copy-paste in the deserializer maps at
/// least two of them onto the wrong key and turns this red. Equal values would
/// have hidden exactly that bug.
#[test]
fn a_full_concurrency_section_attributes_each_budget_to_its_own_field() {
    let section = concurrency(
        "\n[concurrency]\n\
         max-suggestion-requests = 4\n\
         max-action-requests = 2\n\
         max-background-tasks = 7\n\
         max-catalog-tasks = 1\n",
    );

    assert_eq!(
        section,
        ConcurrencySection {
            max_suggestion_requests: Some(4),
            max_action_requests: Some(2),
            max_background_tasks: Some(7),
            max_catalog_tasks: Some(1),
        }
    );
}

/// Spec 13.5 makes the section optional: a manifest that never mentions
/// concurrency must still load, and must declare nothing rather than inherit a
/// silent cap that would throttle a plugin its author never limited.
#[test]
fn a_manifest_without_a_concurrency_section_declares_no_budget() {
    let manifest = parse("");

    assert_eq!(manifest.concurrency.max_suggestion_requests, None);
    assert_eq!(manifest.concurrency.max_action_requests, None);
    assert_eq!(manifest.concurrency.max_background_tasks, None);
    assert_eq!(manifest.concurrency.max_catalog_tasks, None);
    assert_eq!(manifest.concurrency, ConcurrencySection::default());
}

/// Budgets are independent declarations. Naming two must not synthesise the
/// other two: a plugin that caps its suggestions has said nothing about its
/// background work, and capping it anyway would stall unrelated tasks.
#[test]
fn a_partial_concurrency_section_populates_only_the_declared_keys() {
    let section = concurrency(
        "\n[concurrency]\n\
         max-action-requests = 3\n\
         max-catalog-tasks = 9\n",
    );

    assert_eq!(section.max_action_requests, Some(3));
    assert_eq!(section.max_catalog_tasks, Some(9));
    assert_eq!(section.max_suggestion_requests, None);
    assert_eq!(section.max_background_tasks, None);
}

/// A misspelled budget key is the dangerous failure: accepted-and-ignored, the
/// plugin the author meant to cap runs unlimited. `deny_unknown_fields` turns
/// that into a load-time error the author can see and fix.
#[test]
fn an_unknown_key_inside_the_concurrency_section_is_rejected() {
    let text = manifest_text(
        "\n[concurrency]\n\
         max-suggestion-requests = 2\n\
         max-suggestion-request = 1\n",
    );

    let error =
        Manifest::parse(&text).expect_err("a misspelled concurrency budget must not be silently ignored");
    let rendered = error.to_string();
    assert!(
        rendered.contains("max-suggestion-request"),
        "the rejection must name the offending key, got: {rendered}"
    );
}

/// Snake-case is not the manifest dialect (spec 19.1 is kebab-case throughout).
/// Accepting both spellings would give the same budget two names and let a
/// half-migrated deserializer read one file two ways.
#[test]
fn a_snake_case_concurrency_key_is_rejected() {
    let text = manifest_text(
        "\n[concurrency]\n\
         max_background_tasks = 5\n",
    );

    assert!(
        Manifest::parse(&text).is_err(),
        "snake_case keys must not be accepted alongside the kebab-case dialect"
    );
}

/// `= 0` is a declaration, not an omission: it means "never permit this kind of
/// work", which is how an author disables a plugin's action surface while
/// leaving the rest alive. It must not deserialize into the same `None` an
/// absent key produces, or the disabled surface would run unlimited.
#[test]
fn an_explicit_zero_budget_differs_from_an_absent_budget() {
    let declared = concurrency(
        "\n[concurrency]\n\
         max-suggestion-requests = 0\n",
    );
    let absent = concurrency("");

    assert_eq!(declared.max_suggestion_requests, Some(0));
    assert_eq!(absent.max_suggestion_requests, None);
    assert_ne!(declared, absent);
}

/// Budgets are unsigned counts of in-flight work. Negative, fractional and
/// quoted values must fail at the type level rather than being truncated or
/// coerced into a limit the author never wrote.
#[test]
fn negative_fractional_and_quoted_budgets_are_parse_errors() {
    for value in ["-1", "1.5", "\"4\""] {
        let text = manifest_text(&format!(
            "\n[concurrency]\n\
             max-background-tasks = {value}\n"
        ));
        assert!(
            Manifest::parse(&text).is_err(),
            "`max-background-tasks = {value}` must not parse"
        );
    }
}

/// The budgets are `u32`; a value past that ceiling is a typo rather than an
/// enormous limit, and silently wrapping it would uncap the plugin.
#[test]
fn a_budget_beyond_the_u32_ceiling_is_a_parse_error() {
    let text = manifest_text(&format!(
        "\n[concurrency]\n\
         max-action-requests = {}\n",
        u64::from(u32::MAX) + 1
    ));

    assert!(
        Manifest::parse(&text).is_err(),
        "a budget past u32::MAX must be refused, not wrapped"
    );
}

/// The upper edge of the declared integer type is valid and must survive
/// parsing without truncation. The supervisor owns any separate policy cap;
/// the manifest layer must preserve this `u32` declaration exactly.
#[test]
fn a_budget_at_the_u32_ceiling_is_preserved() {
    let section = concurrency(&format!(
        "\n[concurrency]\n\
         max-catalog-tasks = {}\n",
        u32::MAX
    ));
    assert_eq!(section.max_catalog_tasks, Some(u32::MAX));
}

/// The concurrency section rides alongside the other spec 19.1 tables; adding
/// it must not disturb them, and they must not swallow its keys.
#[test]
fn concurrency_coexists_with_the_other_manifest_sections() {
    let manifest = parse(
        "\n[query]\n\
         debounce-ms = 40\n\
         max-concurrent-requests = 2\n\
         \n[concurrency]\n\
         max-suggestion-requests = 6\n",
    );

    assert_eq!(manifest.query.debounce_ms, Some(40));
    assert_eq!(manifest.concurrency.max_suggestion_requests, Some(6));
}
