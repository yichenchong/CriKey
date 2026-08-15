//! Behavioural contract for the default ranker (spec 11.3, roadmap M1).
//!
//! These tests are deliberately weight-free: they never assert a specific
//! constant, only the relative orderings and invariants a user can observe in
//! the result list. That keeps the ranker free to retune its curves without
//! rewriting the suite, while still failing loudly when a signal stops
//! contributing, changes sign, or lets a non-finite value into the ordering key.

use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{DefaultMatcher, MatchMethod, MatchOutcome, Matcher, NormalizedQuery};
use crikey_ranking::{DefaultRanker, HistoryPolicy, Ranker, RankingSignals, Score, SelectionHistory};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn ranker(history_enabled: bool) -> DefaultRanker {
    DefaultRanker::new(HistoryPolicy {
        enabled: history_enabled,
    })
}

/// A deliberately mid-range non-history signal set: each independently tested
/// signal can move without sitting on a clamp boundary.
fn base() -> RankingSignals {
    RankingSignals {
        match_quality: 0.5,
        exact_prefix: false,
        match_position: Some(4),
        category_weight: 0.5,
        plugin_score_hint: 0,
        selection_frequency: 0,
        selection_recency_secs: None,
        query_history: 0.0,
        context_match: false,
        user_preference: 0.0,
    }
}

fn item(label: &str) -> Item {
    Item {
        stable_id: ItemId(format!("dev.crikey.test::{label}")),
        plugin_id: PluginId("dev.crikey.test".into()),
        category: Category::Application,
        label: label.to_owned(),
        description: String::new(),
        target: format!("/usr/bin/{label}"),
        search_terms: vec![label.to_ascii_lowercase()],
        icon_reference: None,
        argument_policy: ArgumentPolicy::default(),
        hit_policy: HitPolicy::default(),
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn query(raw: &str) -> NormalizedQuery {
    NormalizedQuery {
        raw: raw.to_owned(),
        normalized: raw.to_ascii_lowercase(),
        tokens: raw.split_whitespace().map(str::to_ascii_lowercase).collect(),
    }
}

fn outcome(score: f32, method: MatchMethod, highlight_start: usize) -> MatchOutcome {
    MatchOutcome {
        score,
        method,
        highlights: vec![(highlight_start, highlight_start + 2)],
    }
}

fn scores(r: &DefaultRanker, ladder: &[RankingSignals]) -> Vec<Score> {
    ladder.iter().map(|s| r.score_signals(*s)).collect()
}

fn assert_strictly_increasing(what: &str, ladder: &[Score]) {
    for (i, pair) in ladder.windows(2).enumerate() {
        assert!(
            pair[0] < pair[1],
            "{what}: rung {i} must score strictly below rung {}, got {:?} then {:?}",
            i + 1,
            pair[0],
            pair[1]
        );
    }
}

fn assert_non_increasing(what: &str, ladder: &[Score]) {
    for (i, pair) in ladder.windows(2).enumerate() {
        assert!(
            pair[0] >= pair[1],
            "{what}: rung {i} must not score below rung {}, got {:?} then {:?}",
            i + 1,
            pair[0],
            pair[1]
        );
    }
}

fn assert_non_decreasing(what: &str, ladder: &[Score]) {
    for (i, pair) in ladder.windows(2).enumerate() {
        assert!(
            pair[0] <= pair[1],
            "{what}: rung {i} must not score above rung {}, got {:?} then {:?}",
            i + 1,
            pair[0],
            pair[1]
        );
    }
}

// ---------------------------------------------------------------------------
// Textual signals (spec 11.3: match quality, exact whole-label match, position, category)
// ---------------------------------------------------------------------------

#[test]
fn better_textual_match_quality_always_scores_higher() {
    let r = ranker(true);
    let ladder: Vec<_> = [0.0, 0.25, 0.5, 0.75, 1.0]
        .into_iter()
        .map(|match_quality| RankingSignals {
            match_quality,
            ..base()
        })
        .collect();
    assert_strictly_increasing("match quality", &scores(&r, &ladder));
}

#[test]
fn an_exact_whole_label_match_outranks_the_same_match_without_one() {
    let r = ranker(true);
    let plain = r.score_signals(base());
    let prefixed = r.score_signals(RankingSignals {
        exact_prefix: true,
        ..base()
    });
    assert!(
        prefixed > plain,
        "an exact whole-label match must be preferred: {prefixed:?} vs {plain:?}"
    );
}

#[test]
fn an_earlier_match_position_outranks_a_later_one() {
    let r = ranker(true);
    let ladder: Vec<_> = [0, 1, 3, 12, 64]
        .into_iter()
        .map(|match_position| RankingSignals {
            match_position: Some(match_position),
            ..base()
        })
        .collect();
    let ranked = scores(&r, &ladder);
    assert_non_increasing("match position", &ranked);
    assert!(
        ranked[0] > ranked[1],
        "a match at offset 0 must beat one at offset 1"
    );
    assert!(
        ranked[1] > ranked[4],
        "a match at offset 1 must beat one at offset 64"
    );
}

#[test]
fn a_heavier_category_weight_lifts_the_score() {
    let r = ranker(true);
    let ladder: Vec<_> = [0.0, 0.125, 0.25, 0.5, 1.0]
        .into_iter()
        .map(|category_weight| RankingSignals {
            category_weight,
            ..base()
        })
        .collect();
    assert_strictly_increasing("category weight", &scores(&r, &ladder));
}

#[test]
fn plugin_score_hints_are_signed() {
    let r = ranker(true);
    let ladder: Vec<_> = [-100, -10, -1, 0, 1, 10, 100]
        .into_iter()
        .map(|plugin_score_hint| RankingSignals {
            plugin_score_hint,
            ..base()
        })
        .collect();
    let ranked = scores(&r, &ladder);
    assert_strictly_increasing("plugin score hint", &ranked);

    let neutral = r.score_signals(base());
    let demoted = r.score_signals(RankingSignals {
        plugin_score_hint: -50,
        ..base()
    });
    let promoted = r.score_signals(RankingSignals {
        plugin_score_hint: 50,
        ..base()
    });
    assert!(
        demoted < neutral,
        "a negative hint must demote: {demoted:?} vs {neutral:?}"
    );
    assert!(
        promoted > neutral,
        "a positive hint must promote: {promoted:?} vs {neutral:?}"
    );
}

// History, context, and configured-preference signals (spec 11.3)
// ---------------------------------------------------------------------------

#[test]
fn more_selections_lift_the_score_when_history_is_enabled() {
    let r = ranker(true);
    let ladder: Vec<_> = [0, 1, 4, 16, 256]
        .into_iter()
        .map(|selection_frequency| RankingSignals {
            selection_frequency,
            ..base()
        })
        .collect();
    let ranked = scores(&r, &ladder);
    assert_non_decreasing("selection frequency", &ranked);
    // A single recorded selection is already evidence; it must not be discarded
    // by a threshold before the signal starts counting.
    assert!(ranked[0] < ranked[1], "one selection must beat none");
    assert!(ranked[1] < ranked[4], "256 selections must beat one");
}

#[test]
fn more_recent_selections_outrank_older_ones() {
    let r = ranker(true);
    let ages = [
        Some(0),
        Some(3_600),
        Some(86_400),
        Some(604_800),
        Some(31_536_000),
        None,
    ];
    let ladder: Vec<_> = ages
        .into_iter()
        .map(|selection_recency_secs| RankingSignals {
            selection_frequency: 4,
            selection_recency_secs,
            ..base()
        })
        .collect();
    let ranked = scores(&r, &ladder);
    // Decay may plateau, but it may never invert, and never-selected sits last.
    assert_non_increasing("selection recency", &ranked);
    assert!(
        ranked[0] > ranked[4],
        "a selection seconds ago must beat one a year ago"
    );
    assert!(
        ranked[1] > ranked[3],
        "an hour-old selection must beat a week-old one"
    );
    assert!(
        ranked[0] > ranked[5],
        "a fresh selection must beat never having been selected"
    );
}

#[test]
fn stronger_query_specific_history_lifts_the_score_when_history_is_enabled() {
    let r = ranker(true);
    let ladder: Vec<_> = [0.0, 0.25, 0.5, 0.75, 1.0]
        .into_iter()
        .map(|query_history| RankingSignals {
            query_history,
            ..base()
        })
        .collect();
    assert_strictly_increasing("query-specific history", &scores(&r, &ladder));

    let below_range = r.score_signals(RankingSignals {
        query_history: -1.0,
        ..base()
    });
    let zero = r.score_signals(base());
    assert_eq!(
        below_range, zero,
        "query-specific history below zero must be clamped"
    );

    let one = r.score_signals(RankingSignals {
        query_history: 1.0,
        ..base()
    });
    let above_range = r.score_signals(RankingSignals {
        query_history: 2.0,
        ..base()
    });
    assert_eq!(
        above_range, one,
        "query-specific history above one must be clamped"
    );
}

#[test]
fn a_matching_application_context_lifts_the_score() {
    let r = ranker(true);
    let out_of_context = r.score_signals(base());
    let in_context = r.score_signals(RankingSignals {
        context_match: true,
        ..base()
    });
    assert!(
        in_context > out_of_context,
        "context match must be preferred: {in_context:?} vs {out_of_context:?}"
    );
}

#[test]
fn stronger_configured_user_preference_lifts_the_score_under_either_policy() {
    for history_enabled in [false, true] {
        let r = ranker(history_enabled);
        let ladder: Vec<_> = [0.0, 0.25, 0.5, 0.75, 1.0]
            .into_iter()
            .map(|user_preference| RankingSignals {
                user_preference,
                ..base()
            })
            .collect();
        assert_strictly_increasing("configured user preference", &scores(&r, &ladder));

        let below_range = r.score_signals(RankingSignals {
            user_preference: -1.0,
            ..base()
        });
        let zero = r.score_signals(base());
        assert_eq!(
            below_range, zero,
            "configured preference below zero must be clamped"
        );

        let one = r.score_signals(RankingSignals {
            user_preference: 1.0,
            ..base()
        });
        let above_range = r.score_signals(RankingSignals {
            user_preference: 2.0,
            ..base()
        });
        assert_eq!(
            above_range, one,
            "configured preference above one must be clamped"
        );
    }
}

// ---------------------------------------------------------------------------
// History disablement (spec 11.3: "User-history signals shall be disableable")
// ---------------------------------------------------------------------------

#[test]
fn disabled_history_contributes_exactly_zero() {
    let disabled = ranker(false);
    let saturated = RankingSignals {
        selection_frequency: u32::MAX,
        selection_recency_secs: Some(0),
        query_history: 1.0,
        ..base()
    };
    let none = base();

    assert_eq!(
        disabled.score_signals(saturated),
        disabled.score_signals(none),
        "history must contribute nothing at all once disabled"
    );
    // Disabling gates history only: everything else keeps its contribution, so
    // an item with no history scores the same under either policy.
    assert_eq!(
        disabled.score_signals(none),
        ranker(true).score_signals(none),
        "disabling history must not rescale the remaining signals"
    );
}

#[test]
fn disabled_history_cannot_reorder_a_better_textual_match() {
    let familiar = RankingSignals {
        match_quality: 0.2,
        selection_frequency: 5_000,
        selection_recency_secs: Some(1),
        query_history: 1.0,
        ..base()
    };
    let better_match = RankingSignals {
        match_quality: 0.95,
        ..base()
    };

    let disabled = ranker(false);
    assert!(
        disabled.score_signals(better_match) > disabled.score_signals(familiar),
        "with history off, the stronger textual match must win"
    );

    // The same signals must still be worth something while history is enabled,
    // otherwise the policy switch is a no-op in both directions.
    let enabled = ranker(true);
    assert!(
        enabled.score_signals(familiar) > disabled.score_signals(familiar),
        "enabling history must reward a frequently selected, recently used item"
    );
}

// ---------------------------------------------------------------------------
// Ordering-key hygiene (spec 11.1 stable ordering, 11.6 stable tie-breaking)
// ---------------------------------------------------------------------------

#[test]
fn extreme_signal_values_still_produce_a_finite_score() {
    let r = ranker(true);
    let extremes = [
        RankingSignals {
            match_quality: f32::MAX,
            ..base()
        },
        RankingSignals {
            match_quality: f32::MIN,
            ..base()
        },
        RankingSignals {
            category_weight: f32::MAX,
            ..base()
        },
        RankingSignals {
            category_weight: f32::MIN_POSITIVE,
            ..base()
        },
        RankingSignals {
            match_position: Some(u32::MAX),
            ..base()
        },
        RankingSignals {
            plugin_score_hint: i32::MAX,
            ..base()
        },
        RankingSignals {
            plugin_score_hint: i32::MIN,
            ..base()
        },
        RankingSignals {
            selection_frequency: u32::MAX,
            ..base()
        },
        RankingSignals {
            selection_recency_secs: Some(u64::MAX),
            ..base()
        },
        RankingSignals {
            query_history: f32::MAX,
            ..base()
        },
        RankingSignals {
            user_preference: f32::MAX,
            ..base()
        },
        RankingSignals {
            match_quality: f32::MAX,
            exact_prefix: true,
            match_position: Some(u32::MAX),
            category_weight: f32::MAX,
            plugin_score_hint: i32::MIN,
            selection_frequency: u32::MAX,
            selection_recency_secs: Some(u64::MAX),
            query_history: f32::MAX,
            context_match: true,
            user_preference: f32::MAX,
        },
    ];
    for (i, signals) in extremes.into_iter().enumerate() {
        let score = r.score_signals(signals);
        assert!(
            score.get().is_finite(),
            "extreme case {i} produced a non-finite score: {score:?}"
        );
    }
}

#[test]
fn non_finite_signal_inputs_do_not_poison_the_score() {
    let r = ranker(true);
    let poisoned = [
        RankingSignals {
            match_quality: f32::NAN,
            ..base()
        },
        RankingSignals {
            match_quality: f32::INFINITY,
            ..base()
        },
        RankingSignals {
            match_quality: f32::NEG_INFINITY,
            ..base()
        },
        RankingSignals {
            category_weight: f32::NAN,
            ..base()
        },
        RankingSignals {
            category_weight: f32::INFINITY,
            ..base()
        },
        RankingSignals {
            query_history: f32::NAN,
            ..base()
        },
        RankingSignals {
            user_preference: f32::INFINITY,
            ..base()
        },
    ];
    for (i, signals) in poisoned.into_iter().enumerate() {
        let score = r.score_signals(signals);
        assert!(
            score.get().is_finite(),
            "poisoned case {i} leaked a non-finite value into the ordering key: {score:?}"
        );
    }
}

#[test]
fn identical_signals_score_identically() {
    let r = ranker(true);
    let signals = RankingSignals {
        match_quality: 0.61,
        exact_prefix: true,
        match_position: Some(2),
        category_weight: 0.75,
        plugin_score_hint: -7,
        selection_frequency: 9,
        selection_recency_secs: Some(120),
        query_history: 0.73,
        context_match: true,
        user_preference: 0.64,
    };
    assert_eq!(
        r.score_signals(signals),
        r.score_signals(signals),
        "ranking must be deterministic for stable tie-breaking"
    );
    assert_eq!(
        r.score_signals(signals),
        ranker(true).score_signals(signals),
        "two rankers with the same policy must agree"
    );
}

#[test]
fn score_constructor_enforces_a_finite_totally_ordered_value() {
    assert_eq!(Score::new(0.75).get(), 0.75);
    for non_finite in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let score = Score::new(non_finite);
        assert_eq!(
            score,
            Score::new(0.0),
            "a non-finite constructor input must map to the neutral score"
        );
        assert!(score.get().is_finite());
    }

    let mut ordered = [Score::new(1.0), Score::new(-1.0), Score::new(0.0)];
    ordered.sort();
    assert_eq!(ordered, [Score::new(-1.0), Score::new(0.0), Score::new(1.0)]);
}

#[test]
fn score_treats_positive_and_negative_zero_as_the_same_tie() {
    assert_eq!(
        Score::new(-0.0),
        Score::new(0.0),
        "signed zero must not become an accidental ordering key"
    );
    assert_eq!(Score::new(-0.0).get().to_bits(), 0.0f32.to_bits());

    let ranker = ranker(true);
    let negative_zero = RankingSignals {
        match_quality: -0.0,
        category_weight: -0.0,
        query_history: -0.0,
        user_preference: -0.0,
        ..RankingSignals::default()
    };
    assert_eq!(
        ranker.score_signals(negative_zero),
        ranker.score_signals(RankingSignals::default()),
        "equivalent zero-valued signal sets must produce one ordering key"
    );
}

#[test]
fn scores_are_totally_ordered_so_the_result_list_can_sort() {
    let r = ranker(true);
    let mixed = [
        base(),
        RankingSignals {
            match_quality: 1.0,
            exact_prefix: true,
            ..base()
        },
        RankingSignals {
            match_quality: 0.0,
            match_position: Some(u32::MAX),
            ..base()
        },
        RankingSignals {
            plugin_score_hint: i32::MIN,
            ..base()
        },
        RankingSignals {
            selection_frequency: u32::MAX,
            context_match: true,
            ..base()
        },
        RankingSignals {
            category_weight: f32::MAX,
            ..base()
        },
        // A hostile or buggy signal source must not make two results
        // incomparable: `None` from `partial_cmp` is an unsortable result list.
        RankingSignals {
            match_quality: f32::NAN,
            ..base()
        },
        RankingSignals {
            category_weight: f32::NAN,
            ..base()
        },
        RankingSignals {
            match_quality: f32::INFINITY,
            match_position: Some(0),
            ..base()
        },
    ];
    let ranked = scores(&r, &mixed);
    for (i, a) in ranked.iter().enumerate() {
        for (j, b) in ranked.iter().enumerate() {
            assert!(
                a.partial_cmp(b).is_some(),
                "scores {i} and {j} are not comparable: {a:?}, {b:?}"
            );
        }
    }

    let mut sorted = ranked.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_non_increasing("descending sort", &sorted);
    assert_eq!(sorted.len(), ranked.len());
}

// ---------------------------------------------------------------------------
// The Ranker trait: Item + MatchOutcome must feed the same signal set
// ---------------------------------------------------------------------------

#[test]
fn ranker_maps_the_match_outcome_score_to_match_quality() {
    let r = ranker(true);
    let q = query("fi");
    let it = item("Firefox");
    let weak = r.score(&q, &it, &outcome(0.2, MatchMethod::Substring, 4));
    let strong = r.score(&q, &it, &outcome(0.9, MatchMethod::Substring, 4));
    assert!(
        strong > weak,
        "a stronger match outcome must rank higher: {strong:?} vs {weak:?}"
    );
}

#[test]
fn query_method_bands_outrank_the_largest_position_advantage() {
    let r = ranker(true);
    let q = query("fire");
    let it = item("File Reader");
    // One pair per adjacent band edge, strongest first: the stronger method sits
    // at its band FLOOR, the weaker one at its band CEILING. WordPrefix now sits
    // above Substring, so the ladder is
    // ExactPrefix (0.90,1.00) > Prefix (0.75,0.88) > WordPrefix (0.60,0.73)
    // > Substring (0.45,0.58) > Keyword (0.30,0.43) > Fuzzy (0.05,0.17).
    // Adjacent bands are spaced 0.02, which is exactly `W_MATCH_POSITION`. So the
    // largest possible position advantage (offset zero) can close a band edge to a
    // tie -- to within f32 rounding, since 0.45 - 0.43 is not exactly 0.02 in
    // binary -- but it must never carry the weaker method meaningfully past the
    // stronger one, and anything short of the very best position must still lose
    // outright.
    const EDGE_TOLERANCE: f32 = 1e-6;
    let adjacent_band_edges = [
        (MatchMethod::ExactPrefix, 0.90, MatchMethod::Prefix, 0.88),
        (MatchMethod::Prefix, 0.75, MatchMethod::WordPrefix, 0.73),
        (MatchMethod::WordPrefix, 0.60, MatchMethod::Substring, 0.58),
        (MatchMethod::Substring, 0.45, MatchMethod::Keyword, 0.43),
        (MatchMethod::Keyword, 0.30, MatchMethod::Fuzzy, 0.17),
    ];

    for (stronger_method, stronger_floor, weaker_method, weaker_ceiling) in adjacent_band_edges {
        let stronger = r.score(
            &q,
            &it,
            &MatchOutcome {
                score: stronger_floor,
                method: stronger_method,
                highlights: Vec::new(),
            },
        );
        let weaker_at_start = r.score(&q, &it, &outcome(weaker_ceiling, weaker_method, 0));
        assert!(
            stronger.get() >= weaker_at_start.get() - EDGE_TOLERANCE,
            "{stronger_method:?} at its band floor must never be outranked by \
             {weaker_method:?} at its ceiling, even when only the weaker match \
             starts at offset zero: {stronger:?} vs {weaker_at_start:?}"
        );

        // One character further in already spends part of the position budget, so
        // the band separation must reassert itself strictly.
        let weaker_off_start = r.score(&q, &it, &outcome(weaker_ceiling, weaker_method, 1));
        assert!(
            stronger > weaker_off_start,
            "{stronger_method:?} at its band floor must outrank {weaker_method:?} \
             at its ceiling once the weaker match no longer starts at offset zero: \
             {stronger:?} vs {weaker_off_start:?}"
        );
    }
}

#[test]
fn ranker_awards_the_whole_label_bonus_only_to_exact_prefix() {
    let r = ranker(true);
    let q = query("firefox");
    let it = item("Firefox");
    let methods = [
        MatchMethod::ExactPrefix,
        MatchMethod::Prefix,
        MatchMethod::Substring,
        MatchMethod::WordPrefix,
        MatchMethod::Keyword,
        MatchMethod::Fuzzy,
    ];
    let ranked: Vec<_> = methods
        .into_iter()
        .map(|method| r.score(&q, &it, &outcome(0.6, method, 0)))
        .collect();

    for (method, score) in methods.into_iter().zip(ranked.iter()).skip(1) {
        assert!(
            ranked[0] > *score,
            "the exact whole-label method must outrank {method:?} at equal quality"
        );
    }
    for pair in ranked[1..].windows(2) {
        assert_eq!(
            pair[0], pair[1],
            "non-exact methods must not accidentally receive the whole-label bonus"
        );
    }
}

#[test]
fn ranker_prefers_an_earlier_highlight_position() {
    let r = ranker(true);
    let q = query("fox");
    let it = item("Firefox");
    let early = r.score(&q, &it, &outcome(0.6, MatchMethod::Substring, 0));
    let late = r.score(&q, &it, &outcome(0.6, MatchMethod::Substring, 12));
    assert!(
        early > late,
        "an earlier highlight must rank higher: {early:?} vs {late:?}"
    );
}

#[test]
fn an_outcome_without_highlights_receives_no_position_bonus() {
    let r = ranker(true);
    let q = query("fox");
    let it = item("Firefox");
    let localized = r.score(&q, &it, &outcome(0.6, MatchMethod::Keyword, 4));
    let unlocalized = r.score(
        &q,
        &it,
        &MatchOutcome {
            score: 0.6,
            method: MatchMethod::Keyword,
            highlights: Vec::new(),
        },
    );
    assert!(
        localized > unlocalized,
        "missing positional evidence must not receive a start-of-label bonus"
    );
}

#[test]
fn malformed_highlights_do_not_grant_position_evidence() {
    let r = ranker(true);
    let q = query("x");
    let it = item("éx");
    let malformed = r.score(
        &q,
        &it,
        &MatchOutcome {
            score: 0.6,
            method: MatchMethod::Substring,
            // The first range splits `é`; the second is outside the label.
            highlights: vec![(1, 2), (999, 1_000)],
        },
    );
    let unlocalized = r.score(
        &q,
        &it,
        &MatchOutcome {
            score: 0.6,
            method: MatchMethod::Substring,
            highlights: Vec::new(),
        },
    );
    assert_eq!(
        malformed, unlocalized,
        "invalid public ranges must be ignored rather than changing rank"
    );
}

#[test]
fn equal_character_positions_rank_equally_across_utf8_widths() {
    // The default matcher is strict (no subsequence tier), which is all this test
    // needs: "x" lands inside both labels as a plain substring.
    let matcher = DefaultMatcher::default();
    let r = ranker(true);
    let q = query("x");
    let ascii_item = item("ax");
    let unicode_item = item("éx");
    let ascii_outcome = matcher
        .match_item(&q, &ascii_item)
        .expect("the ASCII label must match");
    let unicode_outcome = matcher
        .match_item(&q, &unicode_item)
        .expect("the Unicode label must match");

    assert_eq!(ascii_outcome.method, MatchMethod::Substring);
    assert_eq!(unicode_outcome.method, MatchMethod::Substring);
    assert_eq!(ascii_outcome.highlights[0].0, 1);
    assert_eq!(unicode_outcome.highlights[0].0, 2);
    assert_eq!(
        ascii_outcome.score, unicode_outcome.score,
        "the matcher precondition must isolate byte-width from match quality"
    );
    assert_eq!(
        r.score(&q, &ascii_item, &ascii_outcome),
        r.score(&q, &unicode_item, &unicode_outcome),
        "equal logical character positions must receive equal position bonuses"
    );
}

#[test]
fn ranker_maps_the_item_score_hint() {
    let r = ranker(true);
    let q = query("fi");
    let out = outcome(0.6, MatchMethod::Prefix, 0);

    let neutral = item("Firefox");
    let mut demoted = neutral.clone();
    demoted.score_hint = -50;
    let mut promoted = neutral.clone();
    promoted.score_hint = 50;

    let neutral_score = r.score(&q, &neutral, &out);
    let demoted_score = r.score(&q, &demoted, &out);
    let promoted_score = r.score(&q, &promoted, &out);
    assert!(
        demoted_score < neutral_score,
        "a negative item score hint must demote the item"
    );
    assert!(
        promoted_score > neutral_score,
        "a positive item score hint must promote the item"
    );
}

#[test]
fn ranker_is_deterministic_and_finite() {
    let r = ranker(true);
    let q = query("fi re");
    let mut it = item("Firefox");
    it.score_hint = 25;
    it.category = Category::PluginDefined("browser".into());
    let out = outcome(0.68, MatchMethod::WordPrefix, 3);

    let first = r.score(&q, &it, &out);
    let second = r.score(&q, &it, &out);
    assert_eq!(first, second, "the trait entry point must be deterministic");
    assert!(
        first.get().is_finite(),
        "the trait entry point must not leak a non-finite score: {first:?}"
    );
}

#[test]
fn ranker_history_disablement_reaches_the_trait_entry_point() {
    let q = query("fi");
    let it = item("Firefox");
    let out = outcome(0.6, MatchMethod::Prefix, 0);
    // With no history recorded for this item, both policies must agree; the
    // disabled policy may never invent a contribution of its own.
    assert_eq!(
        ranker(false).score(&q, &it, &out),
        ranker(true).score(&q, &it, &out),
        "an item with no history must score the same under either history policy"
    );
}

#[test]
fn non_prefix_bound_covers_all_optional_signals() {
    let r = ranker(true);
    let candidate = item("candidate");
    let bound = r.non_prefix_upper_bound(&candidate);
    // 0.73 is the WordPrefix ceiling, i.e. the strongest quality any non-prefix
    // method can reach, so this is the worst case the advertised bound must cover.
    let actual = r.score_signals(RankingSignals {
        match_quality: 0.73,
        exact_prefix: false,
        match_position: Some(0),
        category_weight: 1.0,
        plugin_score_hint: candidate.score_hint,
        selection_frequency: u32::MAX,
        selection_recency_secs: Some(0),
        query_history: 1.0,
        context_match: true,
        user_preference: 1.0,
    });

    assert!(
        actual <= bound,
        "the advertised non-prefix upper bound {bound:?} must cover every optional ranking signal, got {actual:?}"
    );
}

#[test]
fn selection_history_augments_frequency_recency_query_and_context() {
    let mut history = SelectionHistory::default();
    let selected = item("Firefox");
    let q = query("fi");
    history.record(&selected, &q, 100);

    let mut signals = RankingSignals::default();
    history.augment(
        &selected,
        &history.affinities_for(&q),
        160,
        Some(&Category::Application),
        &mut signals,
    );
    assert_eq!(signals.selection_frequency, 1);
    assert_eq!(signals.selection_recency_secs, Some(60));
    assert!(signals.query_history > 0.0);
    assert!(signals.context_match);

    let other = item("Terminal");
    let mut neutral = RankingSignals::default();
    history.augment(
        &other,
        &history.affinities_for(&q),
        160,
        Some(&Category::File),
        &mut neutral,
    );
    assert_eq!(neutral.selection_frequency, 0);
    assert_eq!(neutral.selection_recency_secs, None);
    assert_eq!(neutral.query_history, 0.0);
    assert!(!neutral.context_match);
}

/// A snapshot must carry every field the in-memory store holds, because the
/// point of persisting it is that the launcher scores the same way after a
/// restart as before one. Kills a snapshot that keeps frequency and drops the
/// last-selected timestamp or the per-query affinity — a plausible omission,
/// since both are stored separately from the frequency counter and neither is
/// visible in a smoke test that only checks "the item is still remembered".
#[test]
fn a_selection_history_survives_a_snapshot_round_trip_with_every_field_intact() {
    let mut original = SelectionHistory::default();
    let firefox = item("Firefox");
    let terminal = item("Terminal");
    original.record(&firefox, &query("fi"), 1_000);
    original.record(&firefox, &query("fi"), 1_400);
    original.record(&firefox, &query("web"), 1_700);
    original.record(&terminal, &query("te"), 900);

    let restored = SelectionHistory::from_snapshot(original.snapshot());

    for (selected, raw, expected_recency) in [
        (&firefox, "fi", Some(2_000 - 1_700)),
        (&firefox, "web", Some(2_000 - 1_700)),
        (&terminal, "te", Some(2_000 - 900)),
    ] {
        let asked = query(raw);
        let mut before = RankingSignals::default();
        original.augment(
            selected,
            &original.affinities_for(&asked),
            2_000,
            Some(&Category::Application),
            &mut before,
        );
        let mut after = RankingSignals::default();
        restored.augment(
            selected,
            &restored.affinities_for(&asked),
            2_000,
            Some(&Category::Application),
            &mut after,
        );

        assert_eq!(
            after.selection_frequency, before.selection_frequency,
            "restoring must preserve the selection count for {} via {raw}",
            selected.label
        );
        assert_eq!(
            after.selection_recency_secs, expected_recency,
            "restoring must preserve the last-selected timestamp for {} via {raw}",
            selected.label
        );
        assert_eq!(
            after.query_history, before.query_history,
            "restoring must preserve the per-query affinity for {} via {raw}",
            selected.label
        );
    }
}

/// The snapshot itself must be inspectable and complete, not merely
/// round-trippable: a store that persists it has to see one record per
/// selected item and one per (item, query) pair, or it will write a file that
/// is missing exactly what a `from_snapshot` implemented against the same
/// mistake would not notice.
#[test]
fn a_snapshot_exposes_one_record_per_item_and_one_per_query_affinity() {
    let mut history = SelectionHistory::default();
    let firefox = item("Firefox");
    history.record(&firefox, &query("fi"), 10);
    history.record(&firefox, &query("fi"), 20);
    history.record(&firefox, &query("web"), 30);

    let snapshot = history.snapshot();

    assert_eq!(snapshot.selections.len(), 1, "one item was ever selected");
    let selection = &snapshot.selections[0];
    assert_eq!(selection.item, firefox.stable_id);
    assert_eq!(selection.plugin, firefox.plugin_id);
    assert_eq!(selection.frequency, 3);
    assert_eq!(selection.last_selected_secs, Some(30));

    assert_eq!(
        snapshot.query_affinities.len(),
        2,
        "the item was reached through two distinct queries"
    );
    let counts = snapshot
        .query_affinities
        .iter()
        .map(|affinity| (affinity.query.as_str(), affinity.count))
        .collect::<Vec<_>>();
    assert!(counts.contains(&("fi", 2)), "counts were {counts:?}");
    assert!(counts.contains(&("web", 1)), "counts were {counts:?}");
}

/// A cleared history must snapshot as empty, and an empty snapshot must
/// restore as an empty history. Otherwise "clear my ranking history" would be
/// undone by the next save/load pair, which is a privacy failure rather than a
/// ranking one.
#[test]
fn clearing_a_history_leaves_nothing_for_a_snapshot_to_carry() {
    let mut history = SelectionHistory::default();
    history.record(&item("Firefox"), &query("fi"), 10);
    history.clear();

    let snapshot = history.snapshot();
    assert!(snapshot.selections.is_empty());
    assert!(snapshot.query_affinities.is_empty());

    let restored = SelectionHistory::from_snapshot(snapshot);
    let mut signals = RankingSignals::default();
    restored.augment(
        &item("Firefox"),
        &restored.affinities_for(&query("fi")),
        20,
        None,
        &mut signals,
    );
    assert_eq!(signals.selection_frequency, 0);
    assert_eq!(signals.selection_recency_secs, None);
    assert_eq!(signals.query_history, 0.0);
}
