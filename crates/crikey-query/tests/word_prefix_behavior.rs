//! Behavioural contract for word-prefix matching and the opt-in subsequence
//! tier (spec 11.1).
//!
//! The tier exists to make abbreviations work without making coincidences work.
//! `vscode` should find `Visual Studio Code`; `manic` should not find
//! `Memory Diagnostic Tool`. Under the previous always-on subsequence tier both
//! were reported the same way with scores 0.002 apart, so no threshold could
//! separate them — which is why subsequence matching is now opt-in and
//! word-prefix decomposition carries the abbreviation case.
//!
//! Scores are asserted through band membership and relative order, never as
//! exact values, so the quality curves stay free to change.

use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{
    DefaultMatcher, DefaultNormalizer, MatchMethod, MatchOutcome, MatchPolicy, Matcher, NormalizedQuery,
    Normalizer, PreparedLabel, MAX_WORD_PREFIX_TOKEN, MAX_WORD_PREFIX_WORDS,
};

fn item(label: &str, search_terms: &[&str]) -> Item {
    let plugin_id = PluginId("dev.crikey.apps".to_string());
    let category = Category::Application;
    Item {
        stable_id: ItemId::derived(&plugin_id, &category, label),
        plugin_id,
        category,
        label: label.to_string(),
        description: String::new(),
        target: format!("/usr/bin/{label}"),
        search_terms: search_terms.iter().map(|t| (*t).to_string()).collect(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn query(text: &str) -> NormalizedQuery {
    DefaultNormalizer::default().normalize(text)
}

fn matched(label: &str, text: &str) -> Option<MatchOutcome> {
    DefaultMatcher::default().match_item(&query(text), &item(label, &[]))
}

fn method(label: &str, text: &str) -> Option<MatchMethod> {
    matched(label, text).map(|outcome| outcome.method)
}

fn loose_method(label: &str, text: &str) -> Option<MatchMethod> {
    DefaultMatcher::with_subsequence()
        .match_item(&query(text), &item(label, &[]))
        .map(|outcome| outcome.method)
}

/// The highlighted substrings of the raw label, in order.
fn highlighted(label: &str, text: &str) -> Vec<String> {
    let outcome = matched(label, text).expect("expected a match");
    outcome
        .highlights
        .iter()
        .map(|&(start, end)| label[start..end].to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// The motivating regression
// ---------------------------------------------------------------------------

/// By default, a query whose characters merely occur in order is not a match.
///
/// `manic` reads as m-a-n-i-c scattered through `Memory Diagnostic Tool` with no
/// chunk starting a word, so there is no decomposition and the default matcher
/// has nothing weaker to fall back to.
#[test]
fn scattered_subsequence_is_not_a_default_match() {
    assert_eq!(method("Memory Diagnostic Tool", "manic"), None);
    assert_eq!(method("Manage Windows Credentials", "manic"), None);
    assert_eq!(method("Sound Recorder", "code"), None);
    assert_eq!(method("Memory Diagnostic Tool", "mmc"), None);
    assert_eq!(method("Slack", "sc"), None);
}

/// The same inputs are still reachable for a caller that opts in, which is what
/// keeps spec 11.1's fuzzy matching available.
#[test]
fn subsequence_matching_is_available_on_request() {
    assert_eq!(
        loose_method("Memory Diagnostic Tool", "manic"),
        Some(MatchMethod::Fuzzy)
    );
    assert_eq!(loose_method("Sound Recorder", "code"), Some(MatchMethod::Fuzzy));
    // Opting in never *weakens* a stronger reading.
    assert_eq!(
        loose_method("Visual Studio Code", "vscode"),
        Some(MatchMethod::WordPrefix)
    );
}

/// Abbreviations that split on word boundaries match by default.
#[test]
fn word_boundary_abbreviations_match() {
    for (label, text) in [
        ("Visual Studio Code", "vscode"),
        ("Google Chrome", "gochr"),
        ("Registry Editor", "reged"),
        ("Task Manager", "tm"),
        ("Microsoft Management Console", "mmc"),
        ("Windows Media Player", "wmp"),
        ("File Explorer", "fex"),
    ] {
        assert_eq!(
            method(label, text),
            Some(MatchMethod::WordPrefix),
            "{text:?} should be a word-prefix match for {label:?}"
        );
    }
}

/// An initialism is the special case where every chunk is one character, so the
/// tier that replaced acronym matching must still accept one.
#[test]
fn initialisms_are_word_prefix_matches() {
    assert_eq!(method("Task Manager", "tm"), Some(MatchMethod::WordPrefix));
    assert_eq!(method("Google Chrome", "gc"), Some(MatchMethod::WordPrefix));
    assert_eq!(
        method("Microsoft Management Console", "mmc"),
        Some(MatchMethod::WordPrefix)
    );
}

// ---------------------------------------------------------------------------
// Segmentation
// ---------------------------------------------------------------------------

/// Case folding erases camel-case boundaries, so segmentation reads the raw
/// label. Without this `psh` could never reach `PowerShell`: the folded form is
/// `powershell`, a single word with no interior boundary.
#[test]
fn camel_case_boundaries_survive_folding() {
    assert_eq!(method("PowerShell", "psh"), Some(MatchMethod::WordPrefix));
    assert_eq!(highlighted("PowerShell", "psh"), vec!["P", "Sh"]);
    // `vsc` is a literal prefix of the folded `vscode`, and a prefix is the
    // stronger reading; `vc` can only be reached through the camel boundary.
    assert_eq!(method("VSCode", "vsc"), Some(MatchMethod::Prefix));
    assert_eq!(method("VSCode", "vc"), Some(MatchMethod::WordPrefix));
}
/// A letter-to-digit transition opens a word, so a trailing version is
/// addressable.
#[test]
fn letter_to_digit_opens_a_word() {
    assert_eq!(method("Python 3", "py3"), Some(MatchMethod::WordPrefix));
}

/// Highlights point at the chunks that actually matched, in raw-label bytes.
#[test]
fn highlights_cover_the_matched_chunks() {
    assert_eq!(
        highlighted("Visual Studio Code", "vscode"),
        vec!["V", "S", "Code"]
    );
    assert_eq!(highlighted("Google Chrome", "gochr"), vec!["Go", "Chr"]);
    assert_eq!(highlighted("Registry Editor", "reged"), vec!["Reg", "Ed"]);
}

/// Every reported highlight is a valid, non-empty slice of the raw label, even
/// where folding changed byte lengths.
#[test]
fn highlights_are_always_sliceable() {
    for (label, text) in [
        ("Chapter Ⅷ Review", "cvr"),
        ("Straße Manager", "strasse"),
        ("ﬁle Explorer", "filex"),
        ("Visual Studio Code", "vscode"),
        ("Über Menu", "um"),
    ] {
        let Some(outcome) = matched(label, text) else {
            continue;
        };
        for &(start, end) in &outcome.highlights {
            assert!(start < end, "empty highlight for {text:?} on {label:?}");
            assert!(
                label.get(start..end).is_some(),
                "highlight {start}..{end} does not slice {label:?} for {text:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tier boundaries
// ---------------------------------------------------------------------------

/// A single chunk is a substring anchored at a word start, not an abbreviation
/// spanning words, so it is reported as a substring and scored by position.
#[test]
fn single_chunk_is_a_substring_not_a_word_prefix() {
    assert_eq!(method("Visual Studio Code", "code"), Some(MatchMethod::Substring));
    assert_eq!(method("Google Chrome", "chrome"), Some(MatchMethod::Substring));
}

/// Word-prefix outranks substring: an initialism of the whole label is a better
/// reading than two letters buried inside one unrelated word.
///
/// `sc` previously ranked `Discord` above `System Configuration`, because the
/// substring band sat above the acronym band.
#[test]
fn word_prefix_outranks_interior_substring() {
    let configuration = matched("System Configuration", "sc").expect("initialism should match");
    let discord = matched("Discord", "sc").expect("substring should match");
    assert_eq!(configuration.method, MatchMethod::WordPrefix);
    assert_eq!(discord.method, MatchMethod::Substring);
    assert!(
        configuration.score > discord.score,
        "initialism {} should outrank interior substring {}",
        configuration.score,
        discord.score
    );
}

/// A true alias belongs in `search_terms` and reaches the keyword tier.
///
/// `cmd` is not an abbreviation of `Command Prompt` under any word split — the
/// `d` starts no word — so the alias is how it stays reachable.
#[test]
fn aliases_reach_the_keyword_tier() {
    let matcher = DefaultMatcher::default();
    let without = item("Command Prompt", &[]);
    let with = item("Command Prompt", &["cmd"]);
    let normalized = query("cmd");

    assert!(matcher.match_item(&normalized, &without).is_none());
    let outcome = matcher
        .match_item(&normalized, &with)
        .expect("alias should match");
    assert_eq!(outcome.method, MatchMethod::Keyword);
    // Keyword hits are not in the label, so they claim no label highlights.
    assert!(outcome.highlights.is_empty());
}

// ---------------------------------------------------------------------------
// Work caps
// ---------------------------------------------------------------------------

/// Over-cap tokens decline the tier instead of matching a truncated query.
///
/// Truncating would be unsound rather than merely lossy: a partition of the
/// first `n` characters is not a partition of the token, so `vscodezz` would
/// match `Visual Studio Code` as though the trailing characters were never
/// typed.
#[test]
fn over_cap_token_declines_rather_than_truncating() {
    assert_eq!(
        method("Visual Studio Code", "vscode"),
        Some(MatchMethod::WordPrefix)
    );
    assert_eq!(method("Visual Studio Code", "vscodezz"), None);

    let over_cap: String = std::iter::repeat_n('a', MAX_WORD_PREFIX_TOKEN + 1).collect();
    assert_eq!(
        method("Alpha Amber Anchor Apple", &over_cap),
        None,
        "a token over the {MAX_WORD_PREFIX_TOKEN}-character cap must not match"
    );
}

/// A label with more words than the cap declines the tier rather than scoring a
/// prefix of itself.
#[test]
fn over_cap_label_declines() {
    let words: Vec<String> = (0..=MAX_WORD_PREFIX_WORDS)
        .map(|index| format!("Word{index}"))
        .collect();
    let label = words.join(" ");
    // Every chunk would be a word initial, so only the cap can reject this.
    assert_eq!(method(&label, "www"), None);
}

// ---------------------------------------------------------------------------
// Cross-tier invariants
// ---------------------------------------------------------------------------

/// Scores order by method strength across the whole tier stack.
#[test]
fn method_strength_orders_scores() {
    let cases = [
        ("Notepad", "notepad", MatchMethod::ExactPrefix),
        ("Notepad", "note", MatchMethod::Prefix),
        ("Task Manager", "tm", MatchMethod::WordPrefix),
        ("Discord", "sc", MatchMethod::Substring),
    ];
    let mut previous: Option<(MatchMethod, f32)> = None;
    for (label, text, expected) in cases {
        let outcome = matched(label, text).expect("fixture should match");
        assert_eq!(outcome.method, expected, "{text:?} on {label:?}");
        assert!(outcome.score > 0.0 && outcome.score <= 1.0);
        if let Some((previous_method, previous_score)) = previous {
            assert!(
                previous_score > outcome.score,
                "{previous_method:?} must outscore {expected:?}"
            );
        }
        previous = Some((expected, outcome.score));
    }
}

/// A subsequence match scores below every stronger reading.
#[test]
fn subsequence_scores_below_every_stronger_reading() {
    let matcher = DefaultMatcher::with_subsequence();
    let loose = matcher
        .match_item(&query("manic"), &item("Memory Diagnostic Tool", &[]))
        .expect("opt-in matcher should match");
    let substring = matched("Discord", "sc").expect("substring should match");
    assert_eq!(loose.method, MatchMethod::Fuzzy);
    assert!(
        loose.score < substring.score,
        "subsequence {} must score below substring {}",
        loose.score,
        substring.score
    );
}
/// Every token must match, and the outcome takes the *weakest* token so one
/// strong token cannot inflate a poor candidate.
#[test]
fn every_token_must_match() {
    assert_eq!(method("Visual Studio Code", "vscode absent"), None);
    // `visual` is a prefix, `code` only a substring, so the pair is a substring.
    assert_eq!(
        method("Visual Studio Code", "visual code"),
        Some(MatchMethod::Substring)
    );
}

// ---------------------------------------------------------------------------
// Candidate pruning agreement
// ---------------------------------------------------------------------------

/// `may_match` must never reject a candidate the matcher accepts, per policy.
///
/// The catalog prunes with `may_match` before the matcher ever runs, so a
/// disagreement here is a silently lost result rather than a wrong score.
#[test]
fn may_match_admits_everything_the_matcher_accepts() {
    let labels = [
        "Visual Studio Code",
        "Memory Diagnostic Tool",
        "Task Manager",
        "PowerShell",
        "Discord",
        "Python 3",
    ];
    let texts = [
        "vscode", "manic", "tm", "psh", "sc", "code", "note", "py3", "v", "zzz",
    ];
    for (policy, matcher) in [
        (MatchPolicy::Strict, DefaultMatcher::default()),
        (MatchPolicy::Subsequence, DefaultMatcher::with_subsequence()),
    ] {
        for label in labels {
            let candidate = item(label, &[]);
            let prepared = PreparedLabel::new(label);
            for text in texts {
                let normalized = query(text);
                if matcher.match_item(&normalized, &candidate).is_some() {
                    assert!(
                        prepared.may_match_with(&normalized, policy),
                        "{policy:?}: may_match rejected {text:?} on {label:?} \
                         but the matcher accepted it"
                    );
                }
            }
        }
    }
}

/// Strict pruning must not be reused for a subsequence pass.
///
/// This is the failure mode the policy parameter exists to prevent: a strict
/// `may_match` rejects subsequence-only candidates, so narrowing a warm
/// candidate set strictly while matching loosely would drop results the matcher
/// would have accepted.
#[test]
fn strict_pruning_would_drop_subsequence_candidates() {
    let prepared = PreparedLabel::new("Memory Diagnostic Tool");
    let normalized = query("manic");
    assert!(!prepared.may_match_with(&normalized, MatchPolicy::Strict));
    assert!(prepared.may_match_with(&normalized, MatchPolicy::Subsequence));
}
