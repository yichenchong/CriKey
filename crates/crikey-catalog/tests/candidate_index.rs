//! Public-API contract for the catalog's candidate prefilter (spec 11.1,
//! "candidate pruning").
//!
//! The launcher must stop scoring every retained item on every keystroke, but
//! pruning is only ever allowed to change *how much work* a query costs, never
//! *what it answers*. Every test here is therefore written so that a pruning
//! bug surfaces as a missing or a stale item rather than as a timing
//! difference: the load-bearing tests compare the offered candidates against
//! the true [`DefaultMatcher`] match set computed by brute force over the very
//! same slice, so a false negative fails loudly and deterministically.
//!
//! The prefilter is a presence mask. Every method the matcher supports — exact
//! prefix, prefix, word prefix, substring, keyword and the opt-in subsequence
//! tier — can only fire when *every character* of a query token already occurs
//! somewhere in the item's searchable text, so `query_mask & !item_mask == 0`
//! is a necessary condition with no false negatives by construction. Three
//! consequences are defended below. The mask must be taken over **normalized**
//! text, or `Ⅷ` never answers to `viii`. It must be the union over **every**
//! field the matcher reads — label *and* description *and* `search_terms` —
//! because keyword matching reads the latter two, so a label-only mask silently
//! deletes real keyword hits. And it must stop there: `target` is an
//! execution payload the matcher deliberately ignores, so folding it in would
//! undo that exclusion.

use std::collections::{BTreeMap, BTreeSet};

use crikey_catalog::{CatalogError, CatalogStore, CatalogUpdate, MemoryCatalog};
use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{presence_mask, DefaultMatcher, DefaultNormalizer, Matcher, NormalizedQuery, Normalizer};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

/// An item whose only searchable text is what the caller supplies.
///
/// `target` deliberately carries a marker no searchable field does. The
/// matcher excludes execution payloads on purpose, so an index that folds
/// `target` into its mask would offer candidates for `jarvis` that can never
/// match.
fn item(owner: &PluginId, stable_id: &str, label: &str, description: &str, terms: &[&str]) -> Item {
    Item {
        stable_id: ItemId(stable_id.to_owned()),
        plugin_id: owner.clone(),
        category: Category::Application,
        label: label.to_owned(),
        description: description.to_owned(),
        target: format!("/opt/jarvis/{stable_id}"),
        search_terms: terms.iter().map(|term| (*term).to_owned()).collect(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn activate(catalog: &mut MemoryCatalog, owner: &PluginId, instance: u64) {
    catalog
        .activate_instance(owner, instance)
        .expect("a fresh or current high-water instance may activate");
}

fn publish(
    catalog: &mut MemoryCatalog,
    owner: &PluginId,
    instance: u64,
    update: CatalogUpdate,
    items: Vec<Item>,
) {
    catalog
        .apply(owner, instance, update, items)
        .expect("the active plugin instance may update its catalog");
}

/// A catalog holding exactly `items` for `owner`, published by instance 1.
fn stocked(owner: &PluginId, items: Vec<Item>) -> MemoryCatalog {
    let mut catalog = MemoryCatalog::new();
    activate(&mut catalog, owner, 1);
    publish(&mut catalog, owner, 1, CatalogUpdate::Replace, items);
    catalog
}

/// A desktop-shaped slice. Neither `j` nor `q` occurs in any searchable field,
/// which is what makes "offers nothing" reachable without hand-auditing the
/// matcher for every method it supports.
fn desktop_corpus(owner: &PluginId) -> Vec<Item> {
    vec![
        item(
            owner,
            "browser",
            "Firefox Web Browser",
            "Browse the World Wide Web",
            &["internet", "mozilla"],
        ),
        item(
            owner,
            "terminal",
            "GNOME Terminal",
            "Use the command line",
            &["shell", "console"],
        ),
        item(
            owner,
            "files",
            "Nautilus Files",
            "Access and sort documents",
            &["file manager"],
        ),
        item(
            owner,
            "settings",
            "System Settings",
            "Configure your desktop",
            &["preferences", "control panel"],
        ),
        item(owner, "calc", "Calculator", "Perform sums", &["maths"]),
    ]
}

/// A slice whose text only agrees with a query after NFKC compatibility
/// composition and full case folding: `Ⅷ` is one character that folds to
/// `viii`, `ß` folds to `ss`, and `ﬁ` folds to `fi`.
fn unicode_corpus(owner: &PluginId) -> Vec<Item> {
    vec![
        item(owner, "roman", "Chapter Ⅷ", "Straße ﬁle listing", &["Ünicode"]),
        item(
            owner,
            "plain",
            "Plain Report",
            "nothing unusual here",
            &["ordinary"],
        ),
    ]
}

/// A slice built for the one failure a label-only prefilter cannot survive:
/// the item that matches shares **no character at all** with the query,
/// because the evidence lives in a search term or in the description.
///
/// `vim` and `spreadsheet` are disjoint; so are `gimp` and `artwork`. Keyword
/// matching reads `search_terms` and `description`, so a mask taken over the
/// label alone prunes both of these away and silently loses a real result.
fn disjoint_corpus(owner: &PluginId) -> Vec<Item> {
    vec![
        item(owner, "termonly", "Vim", "", &["spreadsheet"]),
        item(owner, "desconly", "Gimp", "artwork studio", &[]),
        item(owner, "neither", "Bash", "", &[]),
    ]
}

/// The alphabet the exhaustive recall test enumerates. `f` occurs nowhere in
/// [`tiny_corpus`], so the enumeration covers unsatisfiable queries too.
const TINY_ALPHABET: [char; 6] = ['a', 'b', 'c', 'd', 'e', 'f'];

/// A slice whose entire searchable vocabulary is drawn from `a`-`e`, so every
/// query over [`TINY_ALPHABET`] can be enumerated rather than sampled.
///
/// `t5` is deliberately down to a single searchable character while its
/// `target` still spells `/opt/jarvis/t5`: any index that reaches past the
/// searchable fields offers it for `a`, `o`, `t` or `5`.
fn tiny_corpus(owner: &PluginId) -> Vec<Item> {
    vec![
        item(owner, "t1", "abc", "bd", &["ce"]),
        item(owner, "t2", "ba cd", "", &["ae"]),
        item(owner, "t3", "d", "abc", &[]),
        item(owner, "t4", "e ab", "cd", &["b"]),
        item(owner, "t5", "e", "", &[]),
    ]
}

// ---------------------------------------------------------------------------
// oracles
// ---------------------------------------------------------------------------

fn normalize(raw: &str) -> NormalizedQuery {
    DefaultNormalizer::default().normalize(raw)
}

/// The text the matcher is allowed to look at: label, description and search
/// terms. Never `target`, never metadata, never action text.
fn searchable_text(item: &Item) -> String {
    let mut raw = item.label.clone();
    if !item.description.is_empty() {
        raw.push(' ');
        raw.push_str(&item.description);
    }
    for term in &item.search_terms {
        if !term.is_empty() {
            raw.push(' ');
            raw.push_str(term);
        }
    }
    normalize(&raw).normalized
}

fn ids(items: &[Item]) -> Vec<&str> {
    items.iter().map(|item| item.stable_id.0.as_str()).collect()
}

fn id_set<'a>(names: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    names.into_iter().map(str::to_owned).collect()
}

/// Ground truth: the items [`DefaultMatcher`] genuinely matches. The prefilter
/// is never allowed to return less than this.
fn matched_ids(catalog: &MemoryCatalog, owner: &PluginId, query: &NormalizedQuery) -> BTreeSet<String> {
    let matcher = DefaultMatcher::default();
    catalog
        .items(owner)
        .iter()
        .filter(|item| matcher.match_item(query, item).is_some())
        .map(|item| item.stable_id.0.clone())
        .collect()
}

/// The contract's own definition of the answer: every item whose mask admits
/// every query token. An empty token list admits the whole slice.
fn mask_admitted_ids(catalog: &MemoryCatalog, owner: &PluginId, query: &NormalizedQuery) -> BTreeSet<String> {
    catalog
        .items(owner)
        .iter()
        .filter(|item| {
            let item_mask = presence_mask(&searchable_text(item));
            query
                .tokens
                .iter()
                .all(|token| (presence_mask(token) & !item_mask) == 0)
        })
        .map(|item| item.stable_id.0.clone())
        .collect()
}

fn offered_ids(catalog: &MemoryCatalog, owner: &PluginId, query: &NormalizedQuery) -> BTreeSet<String> {
    catalog
        .candidates(owner, query)
        .into_iter()
        .map(|item| item.stable_id.0.clone())
        .collect()
}

/// Every candidate must be a live, distinct item of `owner`, offered in the
/// slice's stable order. This is what turns a stale index entry — one that
/// survived a replace, a merge or an invalidation — into a failure.
fn assert_candidates_are_live(catalog: &MemoryCatalog, owner: &PluginId, raw: &str, found: &[&Item]) {
    let retained = catalog.items(owner);
    let mut previous: Option<usize> = None;
    let mut seen = BTreeSet::new();

    for candidate in found {
        let position = retained
            .iter()
            .position(|held| held.stable_id == candidate.stable_id)
            .unwrap_or_else(|| {
                panic!(
                    "stale candidate: {:?} was offered for {raw:?} but {:?} no longer retains it",
                    candidate.stable_id.0, owner.0
                )
            });
        assert_eq!(
            candidate.plugin_id, *owner,
            "candidate {:?} for {raw:?} escaped its plugin slice",
            candidate.stable_id.0
        );
        assert!(
            seen.insert(candidate.stable_id.0.clone()),
            "candidate {:?} was offered twice for {raw:?}",
            candidate.stable_id.0
        );
        if let Some(previous) = previous {
            assert!(
                previous < position,
                "candidates for {raw:?} are not in stable catalog order: {:?} follows position {previous}",
                candidate.stable_id.0
            );
        }
        previous = Some(position);
    }
}

/// The whole pruning contract for one query: nothing real is lost, nothing
/// stale is offered. Returns `(truth, offered)` so a caller can pin the exact
/// ids it cares about.
fn assert_lossless(
    catalog: &MemoryCatalog,
    owner: &PluginId,
    raw: &str,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let query = normalize(raw);
    let found = catalog.candidates(owner, &query);
    assert_candidates_are_live(catalog, owner, raw, &found);

    let offered: BTreeSet<String> = found.iter().map(|item| item.stable_id.0.clone()).collect();
    let truth = matched_ids(catalog, owner, &query);
    let missing: Vec<&String> = truth.difference(&offered).collect();
    assert!(
        missing.is_empty(),
        "pruning changed the answer for {raw:?}: {missing:?} match but were never offered \
         (offered {offered:?})"
    );

    (truth, offered)
}

fn assert_offers(catalog: &MemoryCatalog, owner: &PluginId, raw: &str, expected: &[&str]) {
    let (truth, offered) = assert_lossless(catalog, owner, raw);
    let expected = id_set(expected.iter().copied());
    assert!(
        expected.is_subset(&truth),
        "fixture drift: {raw:?} was expected to really match {expected:?}, matcher said {truth:?}"
    );
    assert!(
        expected.is_subset(&offered),
        "{raw:?} must offer {expected:?}, offered {offered:?}"
    );
}

fn assert_offers_nothing(catalog: &MemoryCatalog, owner: &PluginId, raw: &str) {
    let (truth, offered) = assert_lossless(catalog, owner, raw);
    assert!(
        truth.is_empty(),
        "fixture drift: {raw:?} was expected to match nothing, matcher said {truth:?}"
    );
    assert!(
        offered.is_empty(),
        "{raw:?} cannot match anything, so pruning must offer nothing; offered {offered:?}"
    );
}

// ---------------------------------------------------------------------------
// query generation
// ---------------------------------------------------------------------------

fn words_of(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Every string of length `1..=max_len` over `alphabet`, deterministically
/// ordered shortest first.
fn strings_up_to(alphabet: &[char], max_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut level = vec![String::new()];

    for _ in 0..max_len {
        let mut next = Vec::with_capacity(level.len() * alphabet.len());
        for prefix in &level {
            for &ch in alphabet {
                let mut grown = prefix.clone();
                grown.push(ch);
                next.push(grown);
            }
        }
        out.extend_from_slice(&next);
        level = next;
    }

    out
}

/// Queries drawn from a realistic slice: whole words, every prefix, every
/// short substring, label initials, adjacent word pairs and cross-item pairs.
///
/// Prefixes reach exact-prefix and prefix matching, interior substrings reach
/// substring matching, initials reach word-prefix matching, and the non-label
/// words reach keyword matching — so the recall claim covers every method the
/// strict matcher can answer with. The opt-in subsequence tier needs no
/// queries of its own: a subsequence hit spans the same characters an interior
/// substring hit does, so the mask condition it must satisfy is the same one.
fn derived_queries(items: &[Item]) -> Vec<String> {
    let mut queries: BTreeSet<String> = BTreeSet::new();

    for item in items {
        let label = normalize(&item.label).normalized;
        let initials: String = words_of(&label)
            .iter()
            .filter_map(|word| word.chars().next())
            .collect();
        if initials.chars().count() >= 2 {
            queries.insert(initials);
        }

        let words = words_of(&searchable_text(item));
        for (index, word) in words.iter().enumerate() {
            queries.insert(word.clone());
            queries.insert(word.to_uppercase());

            let chars: Vec<char> = word.chars().collect();
            for len in 1..=chars.len() {
                queries.insert(chars[..len].iter().collect());
            }
            for start in 0..chars.len() {
                for len in 1..=usize::min(3, chars.len() - start) {
                    queries.insert(chars[start..start + len].iter().collect());
                }
            }
            if let Some(next) = words.get(index + 1) {
                queries.insert(format!("{word} {next}"));
            }
        }
    }

    let leads: Vec<String> = items
        .iter()
        .filter_map(|item| words_of(&normalize(&item.label).normalized).first().cloned())
        .collect();
    for left in &leads {
        for right in &leads {
            queries.insert(format!("{left} {right}"));
        }
    }

    queries.into_iter().collect()
}

// ---------------------------------------------------------------------------
// reaching an item
// ---------------------------------------------------------------------------

#[test]
fn a_label_word_offers_the_item_that_carries_it() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));

    assert_offers(&catalog, &owner, "browser", &["browser"]);
    assert_offers(&catalog, &owner, "terminal", &["terminal"]);
    assert_offers(&catalog, &owner, "calculator", &["calc"]);
    // Case is the query author's business, not the index's.
    assert_offers(&catalog, &owner, "NAUTILUS", &["files"]);
}

#[test]
fn matches_that_exist_only_in_a_description_or_a_search_term_are_offered() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));

    // `configure` and `documents` appear in no label at all.
    assert_offers(&catalog, &owner, "configure", &["settings"]);
    assert_offers(&catalog, &owner, "documents", &["files"]);
    // `mozilla` and `console` are search terms only.
    assert_offers(&catalog, &owner, "mozilla", &["browser"]);
    assert_offers(&catalog, &owner, "console", &["terminal"]);
}

/// The regression a label-only mask fails: the matching item's label shares
/// not one character with the query.
///
/// Both halves are real [`DefaultMatcher`] keyword hits, so pruning them is
/// not a precision trade — it deletes results the user asked for.
#[test]
fn a_match_whose_label_shares_no_character_with_the_query_still_survives() {
    let owner = plugin("disjoint");
    let catalog = stocked(&owner, disjoint_corpus(&owner));

    for (raw, expected) in [("spreadsheet", "termonly"), ("artwork", "desconly")] {
        let label = &catalog
            .get(&owner, &ItemId(expected.to_owned()))
            .expect("the fixture item is retained")
            .label;
        let query_text = normalize(raw).normalized;
        let shared: BTreeSet<char> = normalize(label)
            .normalized
            .chars()
            .filter(|ch| query_text.contains(*ch))
            .collect();
        assert!(
            shared.is_empty(),
            "fixture drift: label {label:?} shares {shared:?} with {raw:?}, so this no longer \
             tests a label-disjoint match"
        );

        // The evidence is a search term for the first pair and a description
        // for the second; either way the mask must cover it.
        let (truth, offered) = assert_lossless(&catalog, &owner, raw);
        assert_eq!(truth, id_set([expected]));
        assert_eq!(
            offered,
            id_set([expected]),
            "{raw:?} must reach {expected:?} through its non-label fields"
        );
    }
}

#[test]
fn very_short_tokens_still_offer_every_real_match() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));

    // A one- or two-character token carries almost no evidence, which is
    // exactly when a prefilter is tempted to guess. It may widen; it may not
    // drop anything.
    let mut checked = 0usize;
    let mut with_hits = 0usize;
    for item in catalog.items(&owner) {
        for word in words_of(&searchable_text(item)) {
            let chars: Vec<char> = word.chars().collect();
            for len in 1..=usize::min(2, chars.len()) {
                for start in 0..=chars.len() - len {
                    let token: String = chars[start..start + len].iter().collect();
                    let (truth, _) = assert_lossless(&catalog, &owner, &token);
                    checked += 1;
                    if !truth.is_empty() {
                        with_hits += 1;
                    }
                }
            }
        }
    }

    assert!(checked >= 100, "only {checked} short tokens were exercised");
    assert!(
        with_hits >= 20,
        "only {with_hits} short tokens matched anything, so recall was barely tested"
    );
}

#[test]
fn unicode_case_and_nfkc_folding_reach_a_differently_spelled_item() {
    let owner = plugin("unicode");
    let catalog = stocked(&owner, unicode_corpus(&owner));

    // `Ⅷ` is a single character; the mask has to be taken after normalization
    // or `viii` can never reach it.
    assert_offers(&catalog, &owner, "viii", &["roman"]);
    assert_offers(&catalog, &owner, "VIII", &["roman"]);
    assert_offers(&catalog, &owner, "Ⅷ", &["roman"]);
    // `ß` folds to `ss`, in either direction.
    assert_offers(&catalog, &owner, "strasse", &["roman"]);
    assert_offers(&catalog, &owner, "STRASSE", &["roman"]);
    assert_offers(&catalog, &owner, "Straße", &["roman"]);
    // `ﬁ` folds to `fi`.
    assert_offers(&catalog, &owner, "file", &["roman"]);
    assert_offers(&catalog, &owner, "ﬁle", &["roman"]);
    // Precomposed and decomposed spellings of the same search term agree.
    assert_offers(&catalog, &owner, "Ünicode", &["roman"]);
    assert_offers(&catalog, &owner, "U\u{0308}NICODE", &["roman"]);

    // And folding must not cost precision: `plain` holds no `v`, so it is not
    // a candidate for `viii` even though it is a candidate for nothing else
    // to compare against.
    let (_, offered) = assert_lossless(&catalog, &owner, "viii");
    assert_eq!(offered, id_set(["roman"]));
}

// ---------------------------------------------------------------------------
// refusing an item
// ---------------------------------------------------------------------------

#[test]
fn unrelated_queries_offer_nothing() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));

    // No searchable field holds a `q`.
    assert_offers_nothing(&catalog, &owner, "quokka");
    assert_offers_nothing(&catalog, &owner, "q");
    // One unsatisfiable token poisons the whole conjunction.
    assert_offers_nothing(&catalog, &owner, "browser quokka");
}

#[test]
fn stable_ids_are_identity_not_searchable_text() {
    let owner = plugin("tiny");
    let catalog = stocked(&owner, tiny_corpus(&owner));

    // `t1` and `t5` are retained ids whose characters occur in no searchable
    // field, so an index that folds identity into its mask offers them here.
    assert!(catalog.get(&owner, &ItemId("t1".to_owned())).is_some());
    assert_offers_nothing(&catalog, &owner, "t1");
    assert_offers_nothing(&catalog, &owner, "t5");
    assert_offers_nothing(&catalog, &owner, "1");
}

#[test]
fn execution_targets_are_not_searchable_text() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));

    assert!(
        catalog
            .items(&owner)
            .iter()
            .all(|item| item.target.contains("jarvis")),
        "fixture drift: every target must carry the marker this test looks for"
    );
    // The matcher excludes `target` so that every `/usr/bin/...` item does not
    // answer to `usr`; folding it into the mask would undo that.
    assert_offers_nothing(&catalog, &owner, "jarvis");
    assert_offers_nothing(&catalog, &owner, "j");
}

// ---------------------------------------------------------------------------
// conjunctions and the degenerate query
// ---------------------------------------------------------------------------

#[test]
fn multi_token_queries_offer_the_whole_intersection() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));

    // The matcher ANDs its tokens, so the truth for a pair is exactly the
    // intersection of the truths for its parts.
    let web = matched_ids(&catalog, &owner, &normalize("web"));
    let browser = matched_ids(&catalog, &owner, &normalize("browser"));
    let both = matched_ids(&catalog, &owner, &normalize("web browser"));
    assert_eq!(both, &web & &browser);
    assert!(!both.is_empty(), "fixture drift: the pair must match something");

    assert_offers(&catalog, &owner, "web browser", &["browser"]);
    // A pair that spans two fields: `system` is a label word, `your` lives
    // only in the description.
    assert_offers(&catalog, &owner, "system your", &["settings"]);

    // Candidates are conjunctive too: adding a token can only narrow the set.
    let offered_pair = offered_ids(&catalog, &owner, &normalize("web browser"));
    let offered_web = offered_ids(&catalog, &owner, &normalize("web"));
    let offered_browser = offered_ids(&catalog, &owner, &normalize("browser"));
    assert_eq!(offered_pair, &offered_web & &offered_browser);

    // Two tokens no single item can satisfy together.
    let (truth, offered) = assert_lossless(&catalog, &owner, "firefox calculator");
    assert!(truth.is_empty(), "fixture drift: the pair must match nothing");
    assert!(
        offered.is_empty(),
        "no item holds both token alphabets, so nothing should be offered; offered {offered:?}"
    );
}

#[test]
fn a_query_with_no_tokens_offers_the_whole_slice() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));
    let retained = ids(catalog.items(&owner));

    for raw in ["", "   ", "\t\n"] {
        let query = normalize(raw);
        assert!(
            query.tokens.is_empty(),
            "fixture drift: {raw:?} was expected to tokenize to nothing"
        );
        let offered: Vec<&str> = catalog
            .candidates(&owner, &query)
            .into_iter()
            .map(|item| item.stable_id.0.as_str())
            .collect();
        assert_eq!(
            offered, retained,
            "a query with no tokens prunes nothing and must hand back the slice in order"
        );
    }
}

#[test]
fn repeated_calls_agree() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));
    let query = normalize("e");

    let first: Vec<&str> = catalog
        .candidates(&owner, &query)
        .into_iter()
        .map(|item| item.stable_id.0.as_str())
        .collect();
    let second: Vec<&str> = catalog
        .candidates(&owner, &query)
        .into_iter()
        .map(|item| item.stable_id.0.as_str())
        .collect();

    assert_eq!(first, second, "candidate order must not vary between calls");
    assert!(!first.is_empty(), "fixture drift: `e` must offer something");
}

#[test]
fn prefix_lookup_keeps_short_and_unicode_prefixes_lossless() {
    let owner = plugin("fixture.prefix");
    let catalog = stocked(
        &owner,
        vec![
            item(&owner, "alpha", "Alpha", "", &[]),
            item(&owner, "alpine", "Alpine", "", &[]),
            item(&owner, "eclair", "Éclair", "", &[]),
            item(&owner, "beta", "Beta", "", &[]),
        ],
    );

    let mut short = Vec::new();
    catalog.visit_label_prefixes(&owner, "a", |_, item, _| short.push(item.stable_id.0.as_str()));
    assert_eq!(short, ["alpha", "alpine"]);

    let mut unicode = Vec::new();
    catalog.visit_label_prefixes(&owner, "é", |_, item, _| {
        unicode.push(item.stable_id.0.as_str());
    });
    assert_eq!(unicode, ["eclair"]);
}

#[test]
fn prepared_candidate_prefilter_keeps_match_recall() {
    let owner = plugin("fixture.prepared");
    let catalog = stocked(
        &owner,
        vec![
            item(&owner, "atlas", "Fire Atlas", "Launch the map", &[]),
            item(&owner, "reader", "File Reader", "Open documents", &[]),
            item(&owner, "settings", "System Settings", "Your control panel", &[]),
            item(&owner, "keyword", "Blaze Guide", "Wildland fire safety", &[]),
        ],
    );
    let matcher = DefaultMatcher::default();

    for raw in ["fa", "fr", "ss", "fire", "wildland", "é"] {
        let query = normalize(raw);
        let truth: BTreeSet<String> = catalog
            .items(&owner)
            .iter()
            .filter(|item| matcher.match_item(&query, item).is_some())
            .map(|item| item.stable_id.0.clone())
            .collect();
        let mut offered = BTreeSet::new();
        catalog.visit_prepared_candidates(&owner, &query, |_, item, _| {
            offered.insert(item.stable_id.0.clone());
        });
        assert!(
            truth.is_subset(&offered),
            "prepared candidate pruning lost {raw:?}: truth {truth:?}, offered {offered:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// the index tracks the slice
// ---------------------------------------------------------------------------

#[test]
fn replace_drops_the_previous_slice_from_the_index() {
    let owner = plugin("lifecycle");
    let mut catalog = stocked(
        &owner,
        vec![
            item(&owner, "one", "Alfa", "", &[]),
            item(&owner, "two", "Bruno", "", &[]),
        ],
    );
    assert_offers(&catalog, &owner, "alfa", &["one"]);

    publish(
        &mut catalog,
        &owner,
        1,
        CatalogUpdate::Replace,
        vec![
            item(&owner, "two", "Bruno", "", &[]),
            item(&owner, "three", "Cedar", "", &[]),
        ],
    );

    assert_eq!(catalog.len(), 2);
    // `l` and `f` left the slice with `Alfa`; a surviving index entry is the
    // only way this query can still reach anything.
    assert_offers_nothing(&catalog, &owner, "alfa");
    assert_offers(&catalog, &owner, "bruno", &["two"]);
    assert_offers(&catalog, &owner, "cedar", &["three"]);
}

#[test]
fn replacing_with_an_empty_batch_empties_the_index() {
    let owner = plugin("lifecycle");
    let mut catalog = stocked(&owner, vec![item(&owner, "one", "Alfa", "", &[])]);
    assert_offers(&catalog, &owner, "alfa", &["one"]);

    publish(&mut catalog, &owner, 1, CatalogUpdate::Replace, Vec::new());

    assert!(catalog.items(&owner).is_empty());
    assert_offers_nothing(&catalog, &owner, "alfa");
    assert!(
        catalog.candidates(&owner, &normalize("")).is_empty(),
        "an empty slice has no full slice to hand back"
    );
}

#[test]
fn merge_extends_the_index_without_disturbing_it() {
    let owner = plugin("lifecycle");
    let mut catalog = stocked(&owner, vec![item(&owner, "one", "Alfa", "", &[])]);

    publish(
        &mut catalog,
        &owner,
        1,
        CatalogUpdate::Merge,
        vec![item(&owner, "two", "Bruno", "boxed", &["cedar"])],
    );

    assert_eq!(catalog.len(), 2);
    assert_offers(&catalog, &owner, "alfa", &["one"]);
    assert_offers(&catalog, &owner, "bruno", &["two"]);
    assert_offers(&catalog, &owner, "cedar", &["two"]);
    assert_offers(&catalog, &owner, "boxed", &["two"]);
}

#[test]
fn a_duplicate_stable_id_replaces_its_index_entry() {
    let owner = plugin("lifecycle");
    let mut catalog = stocked(&owner, vec![item(&owner, "dup", "Alfa", "first copy", &["mike"])]);
    assert_offers(&catalog, &owner, "mike", &["dup"]);

    publish(
        &mut catalog,
        &owner,
        1,
        CatalogUpdate::Merge,
        vec![item(&owner, "dup", "Bruno", "second copy", &["oscar"])],
    );

    assert_eq!(catalog.len(), 1);
    let retained = catalog
        .get(&owner, &ItemId("dup".to_owned()))
        .expect("the duplicate id is still retained");
    assert_eq!(retained.label, "Bruno");

    // The replacement's text is reachable...
    assert_offers(&catalog, &owner, "bruno", &["dup"]);
    assert_offers(&catalog, &owner, "oscar", &["dup"]);
    // ...and the superseded copy's is not. `l`, `f`, `k` and `m` all left with
    // it, so an entry that survives can only be a stale one.
    assert_offers_nothing(&catalog, &owner, "alfa");
    assert_offers_nothing(&catalog, &owner, "mike");
}

#[test]
fn a_stable_id_repeated_inside_one_batch_indexes_only_the_last_copy() {
    let owner = plugin("lifecycle");
    let catalog = stocked(
        &owner,
        vec![
            item(&owner, "dup", "Alfa", "first copy", &["mike"]),
            item(&owner, "dup", "Bruno", "second copy", &["oscar"]),
        ],
    );

    assert_eq!(catalog.len(), 1);
    assert_offers(&catalog, &owner, "bruno", &["dup"]);
    assert_offers_nothing(&catalog, &owner, "alfa");
    assert_offers_nothing(&catalog, &owner, "mike");
}

#[test]
fn invalidate_clears_the_index_and_republishing_rebuilds_it() {
    let owner = plugin("lifecycle");
    let mut catalog = stocked(
        &owner,
        vec![
            item(&owner, "one", "Alfa", "", &[]),
            item(&owner, "two", "Bruno", "", &[]),
        ],
    );

    catalog.invalidate(&owner);

    assert_eq!(catalog.len(), 0);
    assert!(catalog.items(&owner).is_empty());
    assert_offers_nothing(&catalog, &owner, "alfa");
    assert_offers_nothing(&catalog, &owner, "bruno");
    assert!(
        catalog.candidates(&owner, &normalize("")).is_empty(),
        "an invalidated slice has nothing to hand back, tokens or not"
    );

    // Authorization survived the invalidation, so the same instance rebuilds.
    publish(
        &mut catalog,
        &owner,
        1,
        CatalogUpdate::Replace,
        vec![item(&owner, "three", "Cedar", "", &[])],
    );
    assert_offers(&catalog, &owner, "cedar", &["three"]);
    assert_offers_nothing(&catalog, &owner, "alfa");
}

#[test]
fn retire_keeps_the_index_and_a_rejected_update_never_touches_it() {
    let owner = plugin("lifecycle");
    let mut catalog = stocked(&owner, vec![item(&owner, "one", "Alfa", "", &[])]);
    let before = ids(catalog.items(&owner))
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    catalog
        .retire_instance(&owner, 1)
        .expect("the active instance may retire");

    // Retiring revokes the publisher, not the retained slice.
    assert_eq!(
        ids(catalog.items(&owner)),
        before.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_offers(&catalog, &owner, "alfa", &["one"]);

    // An update from a retired instance is refused, and a refused update must
    // not leave half of itself behind in the index.
    let rejected = catalog.apply(
        &owner,
        1,
        CatalogUpdate::Merge,
        vec![item(&owner, "two", "Bruno", "", &[])],
    );
    assert_eq!(rejected, Err(CatalogError::StaleInstance));
    assert_offers_nothing(&catalog, &owner, "bruno");
    assert_offers(&catalog, &owner, "alfa", &["one"]);

    // Reactivating the same instance lets the slice grow again.
    activate(&mut catalog, &owner, 1);
    publish(
        &mut catalog,
        &owner,
        1,
        CatalogUpdate::Merge,
        vec![item(&owner, "two", "Bruno", "", &[])],
    );
    assert_offers(&catalog, &owner, "bruno", &["two"]);
    assert_offers(&catalog, &owner, "alfa", &["one"]);
}

#[test]
fn an_update_rejected_by_validation_never_touches_the_index() {
    let owner = plugin("lifecycle");
    let mut catalog = stocked(&owner, vec![item(&owner, "one", "Alfa", "", &[])]);
    let stranger = plugin("someone-else");

    // An item owned by another plugin fails the whole batch.
    let rejected = catalog.apply(
        &owner,
        1,
        CatalogUpdate::Merge,
        vec![
            item(&owner, "two", "Bruno", "", &[]),
            item(&stranger, "three", "Cedar", "", &[]),
        ],
    );
    assert_eq!(rejected, Err(CatalogError::OwnerMismatch));

    assert_eq!(catalog.len(), 1);
    assert_offers(&catalog, &owner, "alfa", &["one"]);
    assert_offers_nothing(&catalog, &owner, "bruno");
    assert_offers_nothing(&catalog, &owner, "cedar");
}

// ---------------------------------------------------------------------------
// isolation
// ---------------------------------------------------------------------------

#[test]
fn plugin_slices_are_indexed_independently() {
    let one = plugin("one");
    let two = plugin("two");
    let mut catalog = MemoryCatalog::new();
    activate(&mut catalog, &one, 1);
    activate(&mut catalog, &two, 1);

    // The same stable id in both slices: a shared index would collide on it.
    publish(
        &mut catalog,
        &one,
        1,
        CatalogUpdate::Replace,
        vec![item(&one, "shared", "Alfa", "", &[])],
    );
    publish(
        &mut catalog,
        &two,
        1,
        CatalogUpdate::Replace,
        vec![item(&two, "shared", "Bruno", "", &[])],
    );

    assert_offers(&catalog, &one, "alfa", &["shared"]);
    assert_offers_nothing(&catalog, &one, "bruno");
    assert_offers(&catalog, &two, "bruno", &["shared"]);
    assert_offers_nothing(&catalog, &two, "alfa");

    // Invalidating one owner leaves the other untouched.
    catalog.invalidate(&one);
    assert_offers_nothing(&catalog, &one, "alfa");
    assert_offers(&catalog, &two, "bruno", &["shared"]);
}

#[test]
fn a_plugin_with_no_slice_offers_nothing() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));
    let stranger = plugin("never-published");

    assert!(catalog.candidates(&stranger, &normalize("browser")).is_empty());
    assert!(
        catalog.candidates(&stranger, &normalize("")).is_empty(),
        "there is no slice to hand back for an unknown plugin"
    );
}

// ---------------------------------------------------------------------------
// the contract, exactly
// ---------------------------------------------------------------------------

#[test]
fn candidates_are_exactly_the_mask_admitted_items() {
    let owner = plugin("desktop");
    let catalog = stocked(&owner, desktop_corpus(&owner));

    // Alphanumeric tokens only: `a`-`z` and `0`-`9` own a mask bit each, so
    // the expectation below does not depend on how residual characters are
    // bucketed.
    for raw in [
        "browser",
        "web",
        "mozilla",
        "system",
        "e",
        "cat",
        "sums",
        "web browser",
        "browser terminal",
        "quokka",
    ] {
        let query = normalize(raw);
        assert_eq!(
            offered_ids(&catalog, &owner, &query),
            mask_admitted_ids(&catalog, &owner, &query),
            "{raw:?} must offer exactly the mask-admitted items"
        );
    }
}

// ---------------------------------------------------------------------------
// recall
// ---------------------------------------------------------------------------

/// The load-bearing test.
///
/// The slice's whole searchable vocabulary is drawn from `a`-`e`, so every
/// query over `a`-`f` up to three characters, and every pair of such queries
/// up to two characters, can be *enumerated* rather than sampled. For each one
/// the offered candidates must cover the true [`DefaultMatcher`] match set —
/// prefix, word-prefix, substring and keyword hits alike — and must equal
/// the mask-admitted set the contract specifies. A pruning bug therefore shows
/// up here as a named missing id, never as a speed difference.
#[test]
fn candidate_recall_is_exhaustive_over_a_tiny_alphabet() {
    let owner = plugin("tiny");
    let catalog = stocked(&owner, tiny_corpus(&owner));
    let slice_len = catalog.items(&owner).len();
    assert_eq!(slice_len, 5, "fixture drift: the tiny slice changed size");

    let short = strings_up_to(&TINY_ALPHABET, 2);
    let mut queries = strings_up_to(&TINY_ALPHABET, 3);
    for left in &short {
        for right in &short {
            queries.push(format!("{left} {right}"));
        }
    }

    let mut with_hits = 0usize;
    let mut fully_pruned = 0usize;
    let mut partly_pruned = 0usize;

    for raw in &queries {
        let query = normalize(raw);
        let (truth, offered) = assert_lossless(&catalog, &owner, raw);
        assert_eq!(
            offered,
            mask_admitted_ids(&catalog, &owner, &query),
            "{raw:?} must offer exactly the mask-admitted items"
        );

        if !truth.is_empty() {
            with_hits += 1;
        }
        if offered.is_empty() {
            fully_pruned += 1;
        } else if offered.len() < slice_len {
            partly_pruned += 1;
        }
    }

    assert!(
        queries.len() >= 2000,
        "only {} queries were enumerated",
        queries.len()
    );
    assert!(
        with_hits >= 100,
        "only {with_hits} queries matched anything, so recall was barely tested"
    );
    assert!(
        fully_pruned >= 100,
        "only {fully_pruned} queries pruned the slice away entirely; pruning is not pruning"
    );
    assert!(
        partly_pruned >= 100,
        "only {partly_pruned} queries pruned the slice partially; pruning is not pruning"
    );
}

/// The same recall claim against text a user would actually type at, including
/// the item whose spelling only agrees after folding.
#[test]
fn candidate_recall_holds_across_a_realistic_catalog() {
    let owner = plugin("realistic");
    let mut items = desktop_corpus(&owner);
    items.extend(unicode_corpus(&owner));
    let catalog = stocked(&owner, items);
    let slice_len = catalog.items(&owner).len();

    let queries = derived_queries(catalog.items(&owner));
    let mut with_hits = 0usize;
    let mut pruned = 0usize;

    for raw in &queries {
        let (truth, offered) = assert_lossless(&catalog, &owner, raw);
        if !truth.is_empty() {
            with_hits += 1;
        }
        if offered.len() < slice_len {
            pruned += 1;
        }
    }

    assert!(
        queries.len() >= 300,
        "only {} queries were derived",
        queries.len()
    );
    assert!(
        with_hits >= 100,
        "only {with_hits} queries matched anything, so recall was barely tested"
    );
    assert!(
        pruned >= 50,
        "only {pruned} queries narrowed the slice at all; pruning is not pruning"
    );
}
