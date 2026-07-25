//! Query engine primitives (spec 11).
//!
//! Normalization, tokenization and candidate matching all execute in Rust on
//! the hot path; plugins submit searchable data rather than reimplementing
//! matching.

use crikey_core::Item;

/// A normalized query ready for matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedQuery {
    pub raw: String,
    pub normalized: String,
    pub tokens: Vec<String>,
}

/// Match method used to produce a candidate score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMethod {
    ExactPrefix,
    Prefix,
    Substring,
    Fuzzy,
    Acronym,
    Keyword,
}

/// Score plus the highlight ranges that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchOutcome {
    pub score: f32,
    pub method: MatchMethod,
    /// Byte ranges within the item label, for UI highlighting.
    pub highlights: Vec<(usize, usize)>,
}

/// Unicode normalization, case folding and tokenization (spec 11.1).
pub trait Normalizer {
    fn normalize(&self, raw: &str) -> NormalizedQuery;
}

/// Candidate matching over in-memory indexes.
pub trait Matcher {
    fn match_item(&self, query: &NormalizedQuery, item: &Item) -> Option<MatchOutcome>;
}
