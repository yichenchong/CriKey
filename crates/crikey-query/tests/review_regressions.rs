use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{
    DefaultMatcher, DefaultNormalizer, MatchMethod, MatchOutcome, Matcher, NormalizedQuery, Normalizer,
    PreparedLabel,
};

fn item(label: &str, description: &str, target: &str, search_terms: &[&str]) -> Item {
    let plugin_id = PluginId("dev.crikey.query-regressions".to_string());
    let category = Category::Application;
    Item {
        stable_id: ItemId::derived(&plugin_id, &category, target),
        plugin_id,
        category,
        label: label.to_string(),
        description: description.to_string(),
        target: target.to_string(),
        search_terms: search_terms.iter().map(|term| (*term).to_string()).collect(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn normalize(raw: &str) -> NormalizedQuery {
    DefaultNormalizer::default().normalize(raw)
}

fn try_match(raw: &str, candidate: &Item) -> Option<MatchOutcome> {
    DefaultMatcher::default().match_item(&normalize(raw), candidate)
}

fn matched(raw: &str, candidate: &Item) -> MatchOutcome {
    let outcome = try_match(raw, candidate)
        .unwrap_or_else(|| panic!("query {raw:?} should match label {:?}", candidate.label));
    assert_valid_highlights(&candidate.label, &outcome.highlights);
    outcome
}

fn assert_valid_highlights(label: &str, highlights: &[(usize, usize)]) {
    let mut previous_end = 0;
    for &(start, end) in highlights {
        assert!(start < end, "empty highlight {start}..{end} in {label:?}");
        assert!(end <= label.len(), "highlight exceeds label {label:?}");
        assert!(
            label.is_char_boundary(start) && label.is_char_boundary(end),
            "highlight {start}..{end} splits a character in {label:?}"
        );
        assert!(
            start >= previous_end,
            "highlights are unordered or overlapping in {label:?}"
        );
        previous_end = end;
    }
}

#[test]
fn full_unicode_case_folding_matches_sigma_and_sharp_s() {
    assert_eq!(normalize("ΟΣ").normalized, "οσ");
    assert_eq!(normalize("ος").normalized, "οσ");
    assert_eq!(normalize("Straße").normalized, "strasse");

    let greek = item("ΟΣ", "", "/test/greek", &[]);
    let greek_match = matched("ος", &greek);
    assert_eq!(greek_match.method, MatchMethod::ExactPrefix);
    assert_eq!(greek_match.highlights, [(0, greek.label.len())]);

    let street = item("Straße", "", "/test/street", &[]);
    let exact = matched("STRASSE", &street);
    assert_eq!(exact.method, MatchMethod::ExactPrefix);
    assert_eq!(exact.highlights, [(0, street.label.len())]);

    let expansion = matched("ss", &street);
    assert_eq!(expansion.method, MatchMethod::Substring);
    assert_eq!(expansion.highlights, [(4, 6)]);
    assert_eq!(&street.label[4..6], "ß");
}

#[test]
fn normalization_changes_map_back_to_complete_raw_characters() {
    let ligature = item("oﬃce", "", "/test/ligature", &[]);
    let expanded = matched("ffi", &ligature);
    assert_eq!(expanded.method, MatchMethod::Substring);
    assert_eq!(expanded.highlights, [(1, 4)]);
    assert_eq!(&ligature.label[1..4], "ﬃ");

    let decomposed = item("Cafe\u{0301} noir", "", "/test/decomposed", &[]);
    let composed = matched("é", &decomposed);
    assert_eq!(composed.method, MatchMethod::Substring);
    assert_eq!(composed.highlights, [(3, 6)]);
    assert_eq!(&decomposed.label[3..6], "e\u{0301}");

    let reordered = item("a\u{0315}\u{0300} test", "", "/test/reordered", &[]);
    let combining_mark = matched("\u{0315}", &reordered);
    assert_eq!(combining_mark.method, MatchMethod::Substring);
    assert_eq!(combining_mark.highlights, [(0, 5)]);
}

#[test]
fn malformed_public_query_cannot_bypass_token_validation() {
    let candidate = item("Firefox", "", "/test/firefox", &[]);
    let forged = NormalizedQuery {
        raw: "absent".to_string(),
        normalized: "firefox".to_string(),
        tokens: vec!["absent".to_string()],
    };

    assert!(
        DefaultMatcher::default()
            .match_item(&forged, &candidate)
            .is_none(),
        "an exact normalized string must not bypass contradictory tokens"
    );

    let empty_token = NormalizedQuery {
        raw: "firefox".to_string(),
        normalized: "firefox".to_string(),
        tokens: vec![String::new()],
    };
    assert!(DefaultMatcher::default()
        .match_item(&empty_token, &candidate)
        .is_none());
}

#[test]
fn combining_marks_do_not_create_acronym_boundaries() {
    for (label, query) in [("q\u{0307}x ray", "qx"), ("नमस्ते", "नत")] {
        let candidate = item(label, "", "/test/marks", &[]);
        let outcome = matched(query, &candidate);
        assert_eq!(
            outcome.method,
            MatchMethod::Fuzzy,
            "combining marks inside {label:?} must not create initials"
        );
    }
}

#[test]
fn fuzzy_compactness_counts_characters_instead_of_utf8_bytes() {
    let ascii_gap = item("axb", "", "/test/ascii-gap", &[]);
    let multibyte_gap = item("aβb", "", "/test/multibyte-gap", &[]);

    let ascii = matched("ab", &ascii_gap);
    let multibyte = matched("ab", &multibyte_gap);
    assert_eq!(ascii.method, MatchMethod::Fuzzy);
    assert_eq!(multibyte.method, MatchMethod::Fuzzy);
    assert_eq!(
        ascii.score.to_bits(),
        multibyte.score.to_bits(),
        "one skipped character must have the same compactness regardless of UTF-8 width"
    );
}

#[test]
fn degraded_offset_tail_never_produces_a_false_highlight() {
    let storm = "\u{0315}".repeat(64);
    let label = format!("ok a{storm}\u{0301} tail");
    let candidate = item(&label, "", "/test/mark-storm", &[]);

    let precise_prefix = matched("ok", &candidate);
    assert_eq!(precise_prefix.method, MatchMethod::Prefix);
    assert_eq!(precise_prefix.highlights, [(0, 2)]);

    let unmappable_tail = matched("tail", &candidate);
    assert_eq!(unmappable_tail.method, MatchMethod::Substring);
    assert!(
        unmappable_tail.highlights.is_empty(),
        "a match beyond the alignment budget must be left unhighlighted"
    );
}

#[test]
fn description_is_searchable_but_execution_target_is_not() {
    let candidate = item(
        "Nimbus",
        "Secure workspace browser",
        "/opt/launch-secret-987",
        &[],
    );

    let description = matched("workspace", &candidate);
    assert_eq!(description.method, MatchMethod::Keyword);
    assert!(description.highlights.is_empty());

    assert!(
        try_match("launch-secret-987", &candidate).is_none(),
        "the execution target is payload, not searchable text"
    );
}

#[test]
fn score_bands_enforce_the_full_declared_precedence() {
    let probes = [
        (MatchMethod::ExactPrefix, item("ac", "", "/test/exact", &[])),
        (MatchMethod::Prefix, item("acorn", "", "/test/prefix", &[])),
        (MatchMethod::Substring, item("xacorn", "", "/test/substring", &[])),
        (
            MatchMethod::Acronym,
            item("Alpha Centauri", "", "/test/acronym", &[]),
        ),
        (MatchMethod::Keyword, item("Nimbus", "", "/test/keyword", &["ac"])),
        (MatchMethod::Fuzzy, item("aβc", "", "/test/fuzzy", &[])),
    ];

    let outcomes: Vec<MatchOutcome> = probes
        .iter()
        .map(|(expected, candidate)| {
            let outcome = matched("ac", candidate);
            assert_eq!(outcome.method, *expected);
            outcome
        })
        .collect();

    for pair in outcomes.windows(2) {
        assert!(
            pair[0].score > pair[1].score,
            "{:?} ({}) must outrank {:?} ({})",
            pair[0].method,
            pair[0].score,
            pair[1].method,
            pair[1].score
        );
        assert!(pair[0].method.precedence() < pair[1].method.precedence());
    }
}

#[test]
fn malformed_prepared_label_boundary_does_not_panic() {
    let result =
        std::panic::catch_unwind(|| PreparedLabel::from_searchable_text("é", "é keyword".to_owned(), 1));

    assert!(
        result.is_ok(),
        "a caller-provided folded offset must not panic on a non-character boundary"
    );
    assert_eq!(result.expect("the constructor did not panic").normalized(), "é");
}

#[test]
fn mismatched_prepared_label_does_not_panic_or_emit_invalid_highlights() {
    let candidate = item("z", "", "/test/mismatch", &[]);
    let prepared = PreparedLabel::new("éx");
    let query = normalize("x");
    let result =
        std::panic::catch_unwind(|| DefaultMatcher::default().match_prepared(&query, &candidate, &prepared));

    assert!(
        result.is_ok(),
        "a prepared label from another item must not make score_prepared slice the candidate unsafely"
    );
    let outcome = result
        .expect("matching did not panic")
        .expect("the mismatched prepared label still contains the query");
    assert!(
        outcome
            .highlights
            .iter()
            .all(|&(start, end)| candidate.label.get(start..end).is_some()),
        "mismatched prepared labels must not emit ranges outside the candidate label"
    );
}
