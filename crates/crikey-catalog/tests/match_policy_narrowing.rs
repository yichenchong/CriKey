//! Incremental narrowing must agree with the policy the matcher runs under.
//!
//! `visit_prepared_positions` re-tests every cached position on each keystroke.
//! If that filter is stricter than the matcher, a candidate can match on one
//! keystroke and vanish on the next — results flickering out as the user types,
//! with no way for the caller to tell that pruning caused it.

use crikey_catalog::{CatalogStore, CatalogUpdate, MemoryCatalog};
use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{DefaultMatcher, DefaultNormalizer, MatchMethod, MatchPolicy, Matcher, Normalizer};

fn plugin() -> PluginId {
    PluginId("dev.crikey.apps".to_string())
}

fn item(label: &str) -> Item {
    let owner = plugin();
    let category = Category::Application;
    Item {
        stable_id: ItemId::derived(&owner, &category, label),
        plugin_id: owner,
        category,
        label: label.to_string(),
        description: String::new(),
        target: format!("/usr/bin/{label}"),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: Default::default(),
        actions: Vec::new(),
    }
}

fn catalog(labels: &[&str]) -> MemoryCatalog {
    let mut catalog = MemoryCatalog::new();
    catalog.activate_instance(&plugin(), 1).unwrap();
    catalog
        .apply(
            &plugin(),
            1,
            CatalogUpdate::Replace,
            labels.iter().map(|label| item(label)).collect(),
        )
        .unwrap();
    catalog
}

/// Types `text` one character at a time, narrowing from the previous keystroke's
/// survivors exactly as a caller would, and returns the survivors per keystroke.
fn narrow_while_typing(catalog: &MemoryCatalog, text: &str, policy: MatchPolicy) -> Vec<Vec<String>> {
    let normalizer = DefaultNormalizer::default();
    let mut prior: Option<Vec<usize>> = None;
    let mut per_keystroke = Vec::new();

    for end in text.char_indices().map(|(index, ch)| index + ch.len_utf8()) {
        let query = normalizer.normalize(&text[..end]);
        let mut kept = Vec::new();
        let mut labels = Vec::new();
        match &prior {
            Some(positions) => catalog.visit_prepared_positions_with(
                &plugin(),
                positions,
                &query,
                policy,
                |position, item, _| {
                    kept.push(position);
                    labels.push(item.label.clone());
                },
            ),
            None => catalog.visit_prepared_candidates(&plugin(), &query, |position, item, _| {
                kept.push(position);
                labels.push(item.label.clone());
            }),
        }
        prior = Some(kept);
        per_keystroke.push(labels);
    }
    per_keystroke
}

/// A subsequence-only candidate survives every keystroke under the matching
/// policy.
///
/// `manic` reaches `Memory Diagnostic Tool` only as a subsequence, and every
/// prefix of the query does too, so a `Subsequence` pass must keep it from the
/// first character to the last.
#[test]
fn subsequence_candidate_survives_every_keystroke_under_its_policy() {
    let catalog = catalog(&["Memory Diagnostic Tool", "Notepad", "Task Manager"]);
    let per_keystroke = narrow_while_typing(&catalog, "manic", MatchPolicy::Subsequence);

    for (index, labels) in per_keystroke.iter().enumerate() {
        assert!(
            labels.iter().any(|label| label == "Memory Diagnostic Tool"),
            "lost the subsequence candidate after {} character(s): {labels:?}",
            index + 1
        );
    }

    // And the opt-in matcher does accept the fully typed query, so keeping the
    // candidate was not wasted work.
    let normalizer = DefaultNormalizer::default();
    let outcome = DefaultMatcher::with_subsequence()
        .match_item(&normalizer.normalize("manic"), &item("Memory Diagnostic Tool"))
        .expect("opt-in matcher should match");
    assert_eq!(outcome.method, MatchMethod::Fuzzy);
}

/// Under the strict policy the same candidate is dropped, and that is correct:
/// the strict matcher would not have accepted it either.
#[test]
fn strict_policy_drops_the_candidate_the_strict_matcher_rejects() {
    let catalog = catalog(&["Memory Diagnostic Tool", "Notepad", "Task Manager"]);
    let per_keystroke = narrow_while_typing(&catalog, "manic", MatchPolicy::Strict);

    let final_labels = per_keystroke.last().expect("query has keystrokes");
    assert!(
        !final_labels.iter().any(|label| label == "Memory Diagnostic Tool"),
        "strict narrowing should not retain a subsequence-only candidate"
    );

    let normalizer = DefaultNormalizer::default();
    assert!(
        DefaultMatcher::default()
            .match_item(&normalizer.normalize("manic"), &item("Memory Diagnostic Tool"))
            .is_none(),
        "strict pruning must agree with the strict matcher"
    );
}

/// A word-prefix candidate survives keystroke by keystroke under both policies:
/// every prefix of `vscode` still decomposes over `Visual Studio Code`.
#[test]
fn word_prefix_candidate_survives_under_both_policies() {
    let catalog = catalog(&["Visual Studio Code", "Notepad", "Memory Diagnostic Tool"]);
    for policy in [MatchPolicy::Strict, MatchPolicy::Subsequence] {
        let per_keystroke = narrow_while_typing(&catalog, "vscode", policy);
        for (index, labels) in per_keystroke.iter().enumerate() {
            assert!(
                labels.iter().any(|label| label == "Visual Studio Code"),
                "{policy:?}: lost the word-prefix candidate after {} character(s): {labels:?}",
                index + 1
            );
        }
    }
}

/// Narrowing never admits a candidate an unnarrowed sweep would reject, so the
/// cache cannot invent results either.
#[test]
fn narrowing_is_a_subset_of_a_cold_sweep() {
    let labels = [
        "Visual Studio Code",
        "Memory Diagnostic Tool",
        "Task Manager",
        "Notepad",
        "Discord",
    ];
    let catalog = catalog(&labels);
    let normalizer = DefaultNormalizer::default();

    for text in ["vscode", "manic", "tm", "sc", "note"] {
        for policy in [MatchPolicy::Strict, MatchPolicy::Subsequence] {
            let warm = narrow_while_typing(&catalog, text, policy)
                .last()
                .cloned()
                .unwrap_or_default();
            let query = normalizer.normalize(text);
            let mut cold = Vec::new();
            catalog.visit_prepared_candidates(&plugin(), &query, |_, item, _| {
                cold.push(item.label.clone());
            });
            for label in &warm {
                assert!(
                    cold.contains(label),
                    "{policy:?}: narrowing admitted {label:?} for {text:?} \
                     that a cold sweep rejects"
                );
            }
        }
    }
}
