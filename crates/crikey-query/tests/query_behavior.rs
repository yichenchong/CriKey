//! Behavioural contract for the M1 core-search query slice (spec 11.1).
//!
//! These tests exercise only the public surface of `crikey-query`:
//! `DefaultNormalizer` (Unicode normalization, case normalization,
//! tokenization) and `DefaultMatcher` (exact-prefix, prefix, word-prefix,
//! substring, keyword and opt-in subsequence matching with byte-safe label
//! highlights).
//!
//! Deliberate non-goals: exact score values are never asserted. Scores are
//! only checked for finiteness, bounds and relative ordering so that the
//! ranking model stays free to evolve (spec 11.3, roadmap M1).

use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{
    DefaultMatcher, DefaultNormalizer, MatchMethod, MatchOutcome, Matcher, NormalizedQuery, Normalizer,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A realistic application catalog item. Every fixture keeps `score_hint` at
/// zero and the same category so that comparisons isolate textual quality.
fn app(label: &str, description: &str, target: &str, search_terms: &[&str]) -> Item {
    let plugin_id = PluginId("dev.crikey.apps".to_string());
    let category = Category::Application;
    Item {
        stable_id: ItemId::derived(&plugin_id, &category, target),
        plugin_id,
        category,
        label: label.to_string(),
        description: description.to_string(),
        target: target.to_string(),
        search_terms: search_terms.iter().map(|t| (*t).to_string()).collect(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn firefox() -> Item {
    app(
        "Firefox",
        "Open the web",
        "/usr/bin/firefox",
        &["browser", "www", "internet"],
    )
}

fn vscode() -> Item {
    app(
        "Visual Studio Code",
        "Edit source files",
        "/usr/bin/code",
        &["editor", "ide"],
    )
}

fn terminal() -> Item {
    app(
        "Terminal Emulator",
        "Shell access",
        "/usr/bin/terminal-emulator",
        &["console", "shell"],
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalize(raw: &str) -> NormalizedQuery {
    DefaultNormalizer::default().normalize(raw)
}

fn try_match(raw: &str, item: &Item) -> Option<MatchOutcome> {
    DefaultMatcher::default().match_item(&normalize(raw), item)
}

/// Subsequence ("fuzzy") matching is opt-in: a default matcher never reports
/// `MatchMethod::Fuzzy`, so every loose expectation must name this matcher.
fn try_match_loose(raw: &str, item: &Item) -> Option<MatchOutcome> {
    DefaultMatcher::with_subsequence().match_item(&normalize(raw), item)
}

/// Matches and enforces the invariants that must hold for *every* outcome.
fn matched(raw: &str, item: &Item) -> MatchOutcome {
    assert_outcome(raw, item, try_match(raw, item))
}

/// [`matched`] under the opt-in subsequence policy.
fn matched_loose(raw: &str, item: &Item) -> MatchOutcome {
    assert_outcome(raw, item, try_match_loose(raw, item))
}

fn assert_outcome(raw: &str, item: &Item, outcome: Option<MatchOutcome>) -> MatchOutcome {
    let outcome = outcome.unwrap_or_else(|| panic!("query {raw:?} should match label {:?}", item.label));
    assert_valid_highlights(&item.label, &outcome.highlights);
    assert!(
        outcome.score.is_finite(),
        "score for {raw:?} on {:?} must be finite, got {}",
        item.label,
        outcome.score
    );
    outcome
}

/// Highlights are byte ranges into the label: they must land on character
/// boundaries, stay inside the label and be strictly ordered.
fn assert_valid_highlights(label: &str, highlights: &[(usize, usize)]) {
    let mut previous_end = 0usize;
    for &(start, end) in highlights {
        assert!(start < end, "empty highlight {start}..{end} in {label:?}");
        assert!(
            end <= label.len(),
            "highlight {start}..{end} exceeds {} bytes of {label:?}",
            label.len()
        );
        assert!(
            label.is_char_boundary(start) && label.is_char_boundary(end),
            "highlight {start}..{end} splits a character of {label:?}"
        );
        assert!(
            start >= previous_end,
            "highlights must be ordered and disjoint, got {start}..{end} after {previous_end}"
        );
        previous_end = end;
    }
}

/// Concatenates the highlighted label bytes, case-normalized for comparison
/// against the query. Panics if a range is not byte-safe.
fn highlighted(label: &str, highlights: &[(usize, usize)]) -> String {
    highlights
        .iter()
        .map(|&(start, end)| &label[start..end])
        .collect::<String>()
        .to_lowercase()
}

// ---------------------------------------------------------------------------
// Normalization (spec 11.1: Unicode normalization, case, tokenization)
// ---------------------------------------------------------------------------

#[test]
fn normalizer_applies_nfkc_compatibility_mappings() {
    // Compatibility characters a user can realistically paste or type on a
    // CJK/IME keyboard must fold to their plain equivalents.
    let cases = [
        ("\u{FB01}nder", "finder"), // LATIN SMALL LIGATURE FI
        (
            "\u{FF26}\u{FF49}\u{FF52}\u{FF45}\u{FF46}\u{FF4F}\u{FF58}",
            "firefox",
        ), // fullwidth
        ("\u{2461}", "2"),          // CIRCLED DIGIT TWO
        ("\u{2122}", "tm"),         // TRADE MARK SIGN, pasted from a vendor app name
        ("\u{2126}", "\u{03C9}"),   // OHM SIGN -> GREEK CAPITAL OMEGA -> omega
    ];
    for (raw, expected) in cases {
        let query = normalize(raw);
        assert_eq!(
            query.normalized, expected,
            "NFKC folding of {raw:?} should yield {expected:?}"
        );
        assert_eq!(query.raw, raw, "raw query text must be preserved verbatim");
    }
}

#[test]
fn normalizer_composes_combining_marks() {
    // A decomposed accent typed by a dead-key layout must match the
    // precomposed form stored in a catalog label.
    let decomposed = normalize("cafe\u{0301}");
    let precomposed = normalize("caf\u{00E9}");
    assert_eq!(
        decomposed.normalized, precomposed.normalized,
        "decomposed and precomposed spellings must normalize identically"
    );
    assert_eq!(decomposed.normalized, "caf\u{00E9}");
}

#[test]
fn normalizer_case_folds_beyond_ascii() {
    // An ASCII-only lowercase pass would leave every non-ASCII letter here
    // untouched, so these rows pin down Unicode-aware case normalization.
    let cases = [
        (
            "\u{0391}\u{0398}\u{0397}\u{039D}\u{0391}",
            "\u{03B1}\u{03B8}\u{03B7}\u{03BD}\u{03B1}",
        ), // ΑΘΗΝΑ
        (
            "\u{041F}\u{0420}\u{0418}\u{0412}\u{0415}\u{0422}",
            "\u{043F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}",
        ), // ПРИВЕТ
        ("\u{00C9}COLE", "\u{00E9}cole"), // ÉCOLE
    ];
    for (raw, expected) in cases {
        assert_eq!(
            normalize(raw).normalized,
            expected,
            "case normalization of {raw:?} must be Unicode-aware"
        );
    }
}

#[test]
fn normalizer_splits_on_unicode_whitespace_without_empty_tokens() {
    // Leading/trailing padding, a tab and a NO-BREAK SPACE all separate words.
    let raw = "  Visual\tStudio\u{00A0}Code \n";
    let query = normalize(raw);

    assert_eq!(query.tokens, ["visual", "studio", "code"]);
    assert_eq!(query.raw, raw, "raw query text must be preserved verbatim");

    let resplit: Vec<&str> = query.normalized.split_whitespace().collect();
    assert_eq!(
        resplit, query.tokens,
        "tokens must be exactly the whitespace split of the normalized form"
    );
}

#[test]
fn blank_query_yields_no_tokens_and_matches_nothing() {
    let query = normalize("  \t\n");
    assert!(query.tokens.is_empty(), "blank input must produce no tokens");
    assert!(
        query.normalized.split_whitespace().next().is_none(),
        "blank input must not leave residual tokens in the normalized form"
    );
    assert!(
        DefaultMatcher::default().match_item(&query, &firefox()).is_none(),
        "an empty query is not a match; default listings are the ranker's job"
    );
}

// ---------------------------------------------------------------------------
// Matching (spec 11.1: prefix, word-prefix, substring, keyword, opt-in fuzzy)
// ---------------------------------------------------------------------------

#[test]
fn label_prefix_match_highlights_the_leading_run() {
    let item = firefox();
    let outcome = matched("fire", &item);

    assert!(
        matches!(outcome.method, MatchMethod::ExactPrefix | MatchMethod::Prefix),
        "a leading label match must be reported as a prefix, got {:?}",
        outcome.method
    );
    assert_eq!(outcome.highlights, [(0, 4)]);
    assert_eq!(highlighted(&item.label, &outcome.highlights), "fire");
}

#[test]
fn whole_label_query_reports_exact_prefix_and_outranks_partial_prefix() {
    let exact = app("Code", "Code editing. Redefined.", "/usr/bin/code", &[]);
    let partial = app("Code Editor", "Lightweight text editor", "/usr/bin/codeedit", &[]);

    let exact_outcome = matched("code", &exact);
    let partial_outcome = matched("code", &partial);

    assert_eq!(
        exact_outcome.method,
        MatchMethod::ExactPrefix,
        "a query covering the whole label is an exact prefix"
    );
    assert_eq!(exact_outcome.highlights, [(0, 4)]);
    assert!(
        exact_outcome.score > partial_outcome.score,
        "exact-prefix preference (spec 11.3): {:?} ({}) must outrank {:?} ({})",
        exact.label,
        exact_outcome.score,
        partial.label,
        partial_outcome.score
    );
}

#[test]
fn substring_match_reports_interior_range() {
    let item = firefox();
    let outcome = matched("refo", &item);

    assert_eq!(
        outcome.method,
        MatchMethod::Substring,
        "an interior contiguous run is a substring match, not a prefix"
    );
    assert_eq!(outcome.highlights, [(2, 6)]);
    assert_eq!(highlighted(&item.label, &outcome.highlights), "refo");
}

#[test]
fn word_prefix_match_highlights_word_initials() {
    let item = vscode();
    let outcome = matched("vsc", &item);

    assert_eq!(
        outcome.method,
        MatchMethod::WordPrefix,
        "each character is a prefix of a distinct label word, which is the \
         word-prefix reading and not a mere substring"
    );
    assert_eq!(
        outcome.highlights,
        [(0, 1), (7, 8), (14, 15)],
        "highlights must point at the initial of each word"
    );
    assert_eq!(highlighted(&item.label, &outcome.highlights), "vsc");
}

#[test]
fn fuzzy_match_requires_ordered_characters_and_an_opt_in_matcher() {
    let item = terminal();

    assert!(
        try_match("tmnl", &item).is_none(),
        "a scattered subsequence carries no evidence of intent, so the default \
         matcher must not credit it on {:?}",
        item.label
    );

    let outcome = matched_loose("tmnl", &item);
    assert_eq!(outcome.method, MatchMethod::Fuzzy);
    assert_eq!(
        outcome.highlights.len(),
        4,
        "each fuzzy character contributes a highlight, got {:?}",
        outcome.highlights
    );
    assert_eq!(highlighted(&item.label, &outcome.highlights), "tmnl");

    assert!(
        try_match_loose("lnmt", &item).is_none(),
        "the same characters out of order must not match {:?} even when the \
         caller opts in",
        item.label
    );
}

#[test]
fn search_term_match_reports_keyword_without_label_highlights() {
    let item = firefox();
    assert!(
        !item.label.to_lowercase().contains("browser"),
        "fixture precondition: the keyword is absent from the label"
    );

    let outcome = matched("browser", &item);

    assert_eq!(
        outcome.method,
        MatchMethod::Keyword,
        "a hit that only exists in search_terms is a keyword match"
    );
    assert!(
        outcome.highlights.is_empty(),
        "highlights are label ranges; a keyword absent from the label highlights nothing, got {:?}",
        outcome.highlights
    );
}

#[test]
fn highlight_ranges_are_byte_offsets_on_multibyte_labels() {
    // "Ü" occupies two bytes, so a char-indexed implementation reports 0..4.
    let overview = app("Übersicht", "System overview", "/usr/bin/overview", &[]);
    let prefix = matched("über", &overview);
    assert_eq!(prefix.highlights, [(0, 5)]);
    assert_eq!(&overview.label[0..5], "Über");

    // "É" shifts every later offset by one byte: 12..17, not 11..16.
    let editor = app("Éditeur de texte", "Write notes", "/usr/bin/gedit", &[]);
    let substring = matched("texte", &editor);
    assert_eq!(substring.highlights, [(12, 17)]);
    assert_eq!(&editor.label[12..17], "texte");
    assert_eq!(highlighted(&editor.label, &substring.highlights), "texte");
}

#[test]
fn every_query_token_must_match() {
    let item = vscode();

    let outcome = matched("visual code", &item);
    assert!(
        outcome.highlights.len() >= 2,
        "each matched token contributes highlights, got {:?}",
        outcome.highlights
    );
    assert_eq!(
        highlighted(&item.label, &outcome.highlights),
        "visualcode",
        "highlights must cover both query tokens"
    );

    assert!(
        try_match("visual absentword", &item).is_none(),
        "a query is rejected unless every token matches"
    );
}

#[test]
fn precedence_orders_prefix_word_prefix_substring_then_fuzzy() {
    let prefix_item = app("Terminal", "Command line shell", "/usr/bin/terminal", &[]);
    let word_prefix_item = app(
        "Time Entry Report Manager",
        "Log billable hours",
        "/usr/bin/tereman",
        &[],
    );
    let substring_item = app("Xterm", "Classic X11 console", "/usr/bin/xterm", &[]);
    let fuzzy_item = app("Text Formatter", "Reformat documents", "/usr/bin/textfmt", &[]);

    let prefix = matched("term", &prefix_item);
    let word_prefix = matched("term", &word_prefix_item);
    let substring = matched("term", &substring_item);
    // The weakest tier is unreachable without opting in, which is precisely
    // what keeps it from competing with the readings above.
    assert!(try_match("term", &fuzzy_item).is_none());
    let fuzzy = matched_loose("term", &fuzzy_item);

    assert!(
        matches!(prefix.method, MatchMethod::ExactPrefix | MatchMethod::Prefix),
        "expected a prefix method for {:?}, got {:?}",
        prefix_item.label,
        prefix.method
    );
    assert_eq!(word_prefix.method, MatchMethod::WordPrefix);
    assert_eq!(substring.method, MatchMethod::Substring);
    assert_eq!(fuzzy.method, MatchMethod::Fuzzy);
    assert_eq!(substring.highlights, [(1, 5)]);

    assert!(
        prefix.score > word_prefix.score,
        "prefix {} must outrank word-prefix {}",
        prefix.score,
        word_prefix.score
    );
    assert!(
        word_prefix.score > substring.score,
        "word-prefix {} must outrank substring {}",
        word_prefix.score,
        substring.score
    );
    assert!(
        substring.score > fuzzy.score,
        "substring {} must outrank fuzzy {}",
        substring.score,
        fuzzy.score
    );
}

#[test]
fn unrelated_query_does_not_match() {
    for item in [firefox(), vscode(), terminal()] {
        assert!(
            try_match("qzxwvj", &item).is_none(),
            "unrelated input must not match {:?}",
            item.label
        );
    }
}

#[test]
fn scores_are_finite_and_bounded_for_every_method() {
    let code = app("Code", "Code editing. Redefined.", "/usr/bin/code", &[]);
    let probes: [(&str, Item, MatchMethod); 6] = [
        ("code", code, MatchMethod::ExactPrefix),
        ("fire", firefox(), MatchMethod::Prefix),
        ("vsc", vscode(), MatchMethod::WordPrefix),
        ("refo", firefox(), MatchMethod::Substring),
        ("browser", firefox(), MatchMethod::Keyword),
        ("tmnl", terminal(), MatchMethod::Fuzzy),
    ];

    for (raw, item, method) in probes {
        let outcome = if method == MatchMethod::Fuzzy {
            // Fuzzy is the one tier the default matcher never reports.
            assert!(
                try_match(raw, &item).is_none(),
                "{raw:?} on {:?} must stay unmatched without opting in",
                item.label
            );
            matched_loose(raw, &item)
        } else {
            matched(raw, &item)
        };
        assert_eq!(
            outcome.method, method,
            "fixture {raw:?} on {:?} must exercise {method:?}",
            item.label
        );
        assert!(
            outcome.score > 0.0 && outcome.score <= 1.0,
            "match quality for {raw:?} on {:?} must lie in (0.0, 1.0], got {} via {:?}",
            item.label,
            outcome.score,
            outcome.method
        );
    }
}
