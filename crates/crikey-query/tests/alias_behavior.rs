//! Behavioural contract for user-defined query aliases (spec 21.2).
//!
//! An alias rewrites the query. These tests pin what that means at the seams
//! that matter: the `tokens`/`normalized` invariant every later reader depends
//! on, the entries that are refused because they could never fire safely, and
//! the single-pass rule that makes a cyclic table terminate.

use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{AliasTable, DefaultMatcher, DefaultNormalizer, MatchMethod, Matcher, Normalizer};

fn app(label: &str, search_terms: &[&str]) -> Item {
    Item {
        stable_id: ItemId(label.to_ascii_lowercase()),
        plugin_id: PluginId("apps".to_owned()),
        category: Category::Application,
        label: label.to_owned(),
        description: String::new(),
        target: format!("/usr/bin/{}", label.to_ascii_lowercase()),
        search_terms: search_terms.iter().map(|term| (*term).to_owned()).collect(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn table(pairs: &[(&str, &str)]) -> AliasTable {
    AliasTable::new(pairs.iter().map(|(alias, target)| (*alias, *target)))
}

fn expand(pairs: &[(&str, &str)], raw: &str) -> Vec<String> {
    let query = DefaultNormalizer::default().normalize(raw);
    table(pairs).expand(query).tokens
}

/// The motivating case: two letters from inside one word, which no matching
/// rule can recover, become the words the user meant.
#[test]
fn an_alias_rewrites_the_token_it_names() {
    assert_eq!(expand(&[("ss", "Settings")], "ss"), vec!["settings"]);
}

/// An alias standing for several words produces several tokens, which are then
/// ANDed like any other query.
#[test]
fn a_multi_word_target_becomes_multiple_tokens() {
    assert_eq!(
        expand(&[("vsc", "Visual Studio Code")], "vsc"),
        vec!["visual", "studio", "code"]
    );
}

/// Aliases apply per token, so an alias composes with the rest of the query
/// instead of only working as a whole-query shorthand.
#[test]
fn an_alias_composes_with_surrounding_tokens() {
    assert_eq!(
        expand(&[("ss", "Settings")], "ss display"),
        vec!["settings", "display"]
    );
}

/// Only a whole token is an alias. `ss` must not rewrite the middle of `press`,
/// or every alias would corrupt unrelated words.
#[test]
fn an_alias_does_not_rewrite_part_of_a_token() {
    assert_eq!(expand(&[("ss", "Settings")], "press"), vec!["press"]);
}

/// Both sides fold, so the case a user typed in their config file is not a
/// second thing they have to get right.
#[test]
fn aliases_are_matched_case_insensitively() {
    assert_eq!(expand(&[("SS", "Settings")], "sS"), vec!["settings"]);
}

/// Expansion runs once. A table that refers to itself terminates by
/// construction rather than by a cycle check.
#[test]
fn expansion_is_single_pass_so_cycles_terminate() {
    assert_eq!(expand(&[("a", "b"), ("b", "a")], "a"), vec!["b"]);
    assert_eq!(expand(&[("a", "b"), ("b", "a")], "b"), vec!["a"]);
}

/// `normalized.split_whitespace()` must keep reproducing `tokens`: the matcher
/// and the incremental candidate cache both read the string, not the vector.
#[test]
fn the_normalized_text_still_reproduces_the_tokens() {
    let query = DefaultNormalizer::default().normalize("vsc");
    let expanded = table(&[("vsc", "Visual Studio Code")]).expand(query);
    assert_eq!(
        expanded.normalized.split_whitespace().collect::<Vec<_>>(),
        expanded.tokens
    );
}

/// The raw text is what the user typed and what the UI echoes; rewriting is a
/// matching concern and must not reach back into it.
#[test]
fn expansion_leaves_the_raw_query_alone() {
    let query = DefaultNormalizer::default().normalize("VSC");
    let expanded = table(&[("vsc", "Visual Studio Code")]).expand(query);
    assert_eq!(expanded.raw, "VSC");
    assert_eq!(expanded.tokens, vec!["visual", "studio", "code"]);
}

/// A target that folds away would delete the token rather than replace it,
/// widening the query to match items the user never asked for.
#[test]
fn an_empty_target_is_refused_rather_than_deleting_the_token() {
    assert!(table(&[("ss", "   ")]).is_empty());
    assert_eq!(expand(&[("ss", "   ")], "ss"), vec!["ss"]);
}

/// A multi-word alias can never equal a single token, so storing it would be a
/// silently dead entry in the user's config.
#[test]
fn a_multi_word_alias_is_refused() {
    assert!(table(&[("v s", "Visual Studio Code")]).is_empty());
}

/// An empty table is the common case and must be a pure pass-through.
#[test]
fn an_empty_table_changes_nothing() {
    let query = DefaultNormalizer::default().normalize("chrome");
    let expanded = AliasTable::default().expand(query.clone());
    assert_eq!(expanded, query);
}

/// A query with no aliased token is returned untouched, so ordinary typing
/// pays nothing for a table that happens to be configured.
#[test]
fn an_unaliased_query_is_returned_unchanged() {
    let query = DefaultNormalizer::default().normalize("chrome");
    let expanded = table(&[("ss", "Settings")]).expand(query.clone());
    assert_eq!(expanded, query);
}

/// End to end through the real matcher: the alias reaches the item, and it
/// reaches it as a *label* reading rather than as the weakest tier, because
/// after rewriting there is nothing alias-shaped left in the query.
#[test]
fn an_alias_reaches_the_item_through_the_matcher() {
    let matcher = DefaultMatcher::default();
    let settings = app("Settings", &["gnome-control-center"]);
    let query = DefaultNormalizer::default().normalize("ss");

    assert!(
        matcher.match_item(&query, &settings).is_none(),
        "`ss` matches nothing on its own - that is why an alias is needed"
    );

    let aliased = table(&[("ss", "Settings")]).expand(query);
    let outcome = matcher
        .match_item(&aliased, &settings)
        .expect("the alias names this item");
    assert_eq!(
        outcome.method,
        MatchMethod::ExactPrefix,
        "the rewritten query reproduces the whole label"
    );
    assert!(
        !outcome.highlights.is_empty(),
        "the match is on the label, so it underlines"
    );
}

/// An alias is a rewrite, so the literal reading is deliberately not also
/// tried. This is the cost of the design and is pinned so it cannot regress
/// into a silent OR.
#[test]
fn an_alias_replaces_the_literal_reading() {
    let matcher = DefaultMatcher::default();
    let literal = app("SS Tool", &[]);
    let query = table(&[("ss", "Settings")]).expand(DefaultNormalizer::default().normalize("ss"));

    assert!(
        matcher.match_item(&query, &literal).is_none(),
        "the token `ss` no longer exists once the user has defined it as an alias"
    );
}
