use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{DefaultMatcher, DefaultNormalizer, MatchMethod, Matcher, Normalizer};
use crikey_ranking::{DefaultRanker, Ranker, Score};

fn candidate(id: &str, label: &str, description: &str, search_terms: &[&str]) -> Item {
    Item {
        stable_id: ItemId(id.to_owned()),
        plugin_id: PluginId("dev.crikey.search-pipeline".to_owned()),
        category: Category::Application,
        label: label.to_owned(),
        description: description.to_owned(),
        target: format!("app://{id}"),
        search_terms: search_terms.iter().map(|term| (*term).to_owned()).collect(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

#[test]
fn unicode_raw_query_matches_filters_and_ranks_candidates_deterministically() {
    let raw = "\u{3000}ＦＩＲＥ\u{a0}";
    let query = DefaultNormalizer::default().normalize(raw);
    assert_eq!(query.raw, raw);
    assert_eq!(query.normalized.trim(), "fire");
    assert_eq!(query.tokens, vec!["fire".to_owned()]);

    let candidates = [
        candidate("a-prefix", "Fire Atlas", "Launch the map", &[]),
        candidate("b-prefix", "Fire Atlas", "Launch the map", &[]),
        candidate("substring", "Campfire Notes", "Outdoor notebook", &[]),
        candidate("keyword", "Blaze Guide", "Wildland fire safety", &[]),
        candidate("fuzzy", "File Reader", "Open documents", &[]),
        candidate("nonmatch", "Water Clock", "Track the tides", &["rain"]),
    ];

    let matcher = DefaultMatcher::default();
    assert!(matcher.match_item(&query, &candidates[5]).is_none());

    let ranker = DefaultRanker::default();
    let mut ranked: Vec<(&Item, MatchMethod, Score)> = candidates[..5]
        .iter()
        .map(|item| {
            let outcome = matcher
                .match_item(&query, item)
                .unwrap_or_else(|| panic!("expected {:?} to match", item.stable_id));
            let method = outcome.method;
            let score = ranker.score(&query, item, &outcome);
            (item, method, score)
        })
        .collect();

    ranked.sort_by(|left, right| {
        right
            .2
            .get()
            .total_cmp(&left.2.get())
            .then_with(|| left.0.stable_id.cmp(&right.0.stable_id))
    });

    let ordered_ids: Vec<&str> = ranked
        .iter()
        .map(|(item, _, _)| item.stable_id.0.as_str())
        .collect();
    assert_eq!(
        ordered_ids,
        ["a-prefix", "b-prefix", "substring", "keyword", "fuzzy"]
    );

    assert_eq!(ranked[0].1, MatchMethod::Prefix);
    assert_eq!(ranked[1].1, MatchMethod::Prefix);
    assert_eq!(ranked[2].1, MatchMethod::Substring);
    assert_eq!(ranked[3].1, MatchMethod::Keyword);
    assert_eq!(ranked[4].1, MatchMethod::Fuzzy);
    assert_eq!(ranked[0].2, ranked[1].2);
    assert!(ranked[0].2 > ranked[2].2);
    assert!(ranked[0].2 > ranked[4].2);
    assert!(ranked[2].2 > ranked[3].2);
}
