//! Ranking engine (spec 11.3).

use crikey_core::Item;
use crikey_query::{MatchOutcome, NormalizedQuery};

/// Signals available to the default ranker. History signals are disableable.
#[derive(Debug, Clone, Copy, Default)]
pub struct RankingSignals {
    pub match_quality: f32,
    pub exact_prefix: bool,
    pub match_position: u32,
    pub category_weight: f32,
    pub plugin_score_hint: i32,
    pub selection_frequency: u32,
    pub selection_recency_secs: Option<u64>,
    pub context_match: bool,
}

/// Final ordering key. Ties break deterministically to keep the list stable.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Score(pub f32);

pub trait Ranker {
    fn score(&self, query: &NormalizedQuery, item: &Item, outcome: &MatchOutcome) -> Score;
}

/// User history contributions can be switched off entirely (spec 11.3).
#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryPolicy {
    pub enabled: bool,
}
