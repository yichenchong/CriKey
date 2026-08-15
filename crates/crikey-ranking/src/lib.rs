//! Ranking engine (spec 11.3).
//!
//! [`DefaultRanker`] collapses the signals gathered for one candidate into a
//! single ordering key. Two properties are load bearing:
//!
//! * **Monotonicity.** Improving any one signal never lowers the score, so the
//!   order the user sees always follows the evidence. Every curve is bounded
//!   and saturating, so no single signal can run away with the ranking.
//! * **Hygiene.** Plugin supplied floats are sanitized before any arithmetic,
//!   and [`Score`] stores only finite values behind a total comparison
//!   (spec 11.1, 11.6).
//!
//! The score is a plain weighted sum and is never normalized by the set of
//! active signals: switching a signal off removes exactly its own
//! contribution and rescales nothing else.

use std::{cmp::Ordering, collections::BTreeMap};

use crikey_core::{Category, Item, ItemId, PluginId};
use crikey_query::{MatchMethod, MatchOutcome, MatchSummary, NormalizedQuery};

// ---------------------------------------------------------------------------
// Signal weights. Only the relative magnitudes matter.
// ---------------------------------------------------------------------------

/// Weight of the matcher's own quality score: the dominant signal.
const W_MATCH_QUALITY: f32 = 1.0;
/// Bonus when the whole query reproduces the whole item label.
const W_EXACT_PREFIX: f32 = 0.35;
/// Weight of how early in the item the match landed. Kept below the matcher's
/// narrowest adjacent quality-band gap so position can refine, not invert, it.
const W_MATCH_POSITION: f32 = 0.02;
/// Weight of the item category preference.
const W_CATEGORY: f32 = 0.25;
/// Weight of the plugin's own opinion: the only term that can demote.
const W_PLUGIN_HINT: f32 = 0.2;
/// Weight of how often the user has picked the item.
const W_FREQUENCY: f32 = 0.3;
/// Weight of how recently the user picked it.
const W_RECENCY: f32 = 0.25;
/// Weight of prior selections made for this specific query.
const W_QUERY_HISTORY: f32 = 0.2;
/// Weight of a match against the foreground application context.
const W_CONTEXT: f32 = 0.15;
/// Weight of an explicit configured user preference.
const W_USER_PREFERENCE: f32 = 0.2;

/// Lowest key the ranker can emit: every term except the plugin hint is
/// non-negative, and the hint bottoms out at `-W_PLUGIN_HINT`.
const MIN_SCORE: f32 = -W_PLUGIN_HINT;

/// Highest key the ranker can emit: every term saturated.
///
/// Summed in the same order the score accumulates. Float addition is monotone
/// in each operand, so a real score can never round above this bound and the
/// final clamp can only ever act on a bug, never on a legitimate ranking.
const MAX_SCORE: f32 = W_MATCH_QUALITY
    + W_EXACT_PREFIX
    + W_MATCH_POSITION
    + W_CATEGORY
    + W_PLUGIN_HINT
    + W_FREQUENCY
    + W_RECENCY
    + W_QUERY_HISTORY
    + W_CONTEXT
    + W_USER_PREFERENCE;

/// Match offset that has already surrendered half of `W_MATCH_POSITION`.
const POSITION_HALF_LIFE: f32 = 4.0;
/// Selection count that has earned half of `W_FREQUENCY`.
const FREQUENCY_HALF_LIFE: f32 = 8.0;
/// Age of a selection that still keeps half of `W_RECENCY`: one day.
const RECENCY_HALF_LIFE_SECS: f32 = 86_400.0;
/// Plugin hint magnitude worth half of `W_PLUGIN_HINT`.
const HINT_HALF_SCALE: f32 = 100.0;

/// Signals available to the default ranker. History-derived signals are disableable.
///
/// The ranker treats every field as untrusted: floats outside their documented
/// range, and non-finite floats, are folded onto the range rather than
/// rejected, so a hostile plugin can degrade only its own placement.
#[derive(Debug, Clone, Copy, Default)]
pub struct RankingSignals {
    /// Textual match quality from the matcher, clamped to `0.0..=1.0`.
    /// Higher is better.
    pub match_quality: f32,
    /// The whole query exactly reproduces the whole item label.
    pub exact_prefix: bool,
    /// Character offset of the earliest match, or `None` when there is no
    /// positional evidence. Lower positions are better.
    pub match_position: Option<u32>,
    /// Category preference, clamped to `0.0..=1.0`. Higher is better.
    pub category_weight: f32,
    /// The owning plugin's signed opinion. Negative demotes, positive promotes.
    pub plugin_score_hint: i32,
    /// How many times the user has selected this item. History signal.
    pub selection_frequency: u32,
    /// Seconds since the last selection, or `None` if never selected.
    /// History signal.
    pub selection_recency_secs: Option<u64>,
    /// Prior affinity between this specific query and item, clamped to
    /// `0.0..=1.0`. History signal.
    pub query_history: f32,
    /// The item suits the foreground application context.
    pub context_match: bool,

    /// Configured preference for this item, clamped to `0.0..=1.0`.
    pub user_preference: f32,
}
/// A bounded, deterministic record of selections used by ranking (spec 11.3).
///
/// The store is deliberately owned by the application rather than persisted
/// by the ranker. Callers decide when a selection is durable and provide the
/// current query and clock value, which keeps tests and replay deterministic.
#[derive(Debug, Clone, Default)]
pub struct SelectionHistory {
    entries: BTreeMap<(PluginId, ItemId), HistoryEntry>,
    query_counts: BTreeMap<(PluginId, ItemId, String), u32>,
}

#[derive(Debug, Clone, Copy, Default)]
struct HistoryEntry {
    frequency: u32,
    last_selected_secs: Option<u64>,
}

/// One item's selection record, as it crosses a persistence boundary.
///
/// The in-memory store is keyed maps, which is the right shape for lookup and
/// the wrong shape for a file: a caller writing it out must be able to walk
/// every record without the ranker deciding what a record looks like on disk.
/// So the snapshot flattens the key into the record and stops there — the
/// encoding stays entirely with whoever owns the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRecord {
    pub plugin: PluginId,
    pub item: ItemId,
    pub frequency: u32,
    pub last_selected_secs: Option<u64>,
}

/// One (item, query) affinity count, the second half of the history.
///
/// Separate from [`SelectionRecord`] because it is separately keyed: the same
/// item carries one count per query that ever selected it, and collapsing the
/// two into one record would either lose counts or invent them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAffinityRecord {
    pub plugin: PluginId,
    pub item: ItemId,
    pub query: String,
    pub count: u32,
}

/// A lossless copy of a [`SelectionHistory`].
///
/// Lossless is the whole contract: a snapshot restored into a fresh history
/// must score identically to the one it came from, or persistence would
/// silently rewrite the user's ranking every launch. Every field of every
/// record therefore appears here, including the per-query affinity that a
/// frequency-only snapshot would quietly drop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionHistorySnapshot {
    pub selections: Vec<SelectionRecord>,
    pub query_affinities: Vec<QueryAffinityRecord>,
}

impl SelectionHistory {
    /// Records one successful item selection.
    pub fn record(&mut self, item: &Item, query: &NormalizedQuery, now_secs: u64) {
        let entry = self
            .entries
            .entry((item.plugin_id.clone(), item.stable_id.clone()))
            .or_default();
        entry.frequency = entry.frequency.saturating_add(1);
        entry.last_selected_secs = Some(now_secs);

        let query_key = (
            item.plugin_id.clone(),
            item.stable_id.clone(),
            query.normalized.clone(),
        );
        let count = self.query_counts.entry(query_key).or_default();
        *count = count.saturating_add(1);
    }

    /// Applies the recorded history and foreground category to dynamic signals.
    ///
    /// Future timestamps are treated as zero age rather than underflowing. A
    /// missing history entry leaves all history fields neutral.
    pub fn augment(
        &self,
        item: &Item,
        query: &NormalizedQuery,
        now_secs: u64,
        foreground_category: Option<&Category>,
        signals: &mut RankingSignals,
    ) {
        if let Some(entry) = self
            .entries
            .get(&(item.plugin_id.clone(), item.stable_id.clone()))
        {
            signals.selection_frequency = entry.frequency;
            signals.selection_recency_secs = entry
                .last_selected_secs
                .map(|selected| now_secs.saturating_sub(selected));
        }
        signals.query_history = self
            .query_counts
            .get(&(
                item.plugin_id.clone(),
                item.stable_id.clone(),
                query.normalized.clone(),
            ))
            .copied()
            .map_or(0.0, |count| saturating_rise(count as f32, FREQUENCY_HALF_LIFE));
        signals.context_match = foreground_category.is_some_and(|category| category == &item.category);
    }

    /// Removes all records. Useful when the user clears ranking history.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.query_counts.clear();
    }

    /// Copies every record out, in the store's own deterministic key order.
    ///
    /// Ordering is the `BTreeMap`'s, so two runs that recorded the same
    /// selections produce byte-identical snapshots. That is what lets a caller
    /// compare or diff a persisted history instead of only overwriting it.
    pub fn snapshot(&self) -> SelectionHistorySnapshot {
        SelectionHistorySnapshot {
            selections: self
                .entries
                .iter()
                .map(|((plugin, item), entry)| SelectionRecord {
                    plugin: plugin.clone(),
                    item: item.clone(),
                    frequency: entry.frequency,
                    last_selected_secs: entry.last_selected_secs,
                })
                .collect(),
            query_affinities: self
                .query_counts
                .iter()
                .map(|((plugin, item, query), count)| QueryAffinityRecord {
                    plugin: plugin.clone(),
                    item: item.clone(),
                    query: query.clone(),
                    count: *count,
                })
                .collect(),
        }
    }

    /// Rebuilds a history from a snapshot.
    ///
    /// Duplicate keys keep the last record rather than being rejected: the
    /// snapshot may have come off disk, where nothing prevents a damaged or
    /// hand-edited file from repeating a key, and a ranking store has no
    /// business refusing to start over an ambiguity it can resolve.
    pub fn from_snapshot(snapshot: SelectionHistorySnapshot) -> Self {
        let mut history = Self::default();
        for record in snapshot.selections {
            history.entries.insert(
                (record.plugin, record.item),
                HistoryEntry {
                    frequency: record.frequency,
                    last_selected_secs: record.last_selected_secs,
                },
            );
        }
        for record in snapshot.query_affinities {
            history
                .query_counts
                .insert((record.plugin, record.item, record.query), record.count);
        }
        history
    }
}

/// Final ordering key. Candidate identity supplies any deterministic tie-break.
///
/// Only finite values are stored, and [`Ord`] provides a total comparison.
/// Higher is better.
#[derive(Debug, Clone, Copy)]
pub struct Score(f32);

impl Score {
    /// Builds an ordering key, mapping an unusable non-finite value to neutral.
    #[must_use]
    pub fn new(value: f32) -> Self {
        let value = if value.is_finite() {
            // IEEE-754 has two zero encodings. They compare equal as scores;
            // canonicalise them so total_cmp does not invent a tie-break that
            // callers never supplied.
            if value == 0.0 {
                0.0
            } else {
                value
            }
        } else {
            0.0
        };
        Self(value)
    }

    /// Returns the finite numeric value of this ordering key.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl PartialEq for Score {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}

impl Eq for Score {}

impl PartialOrd for Score {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Score {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Turns a matched candidate into an ordering key.
pub trait Ranker {
    fn score(&self, query: &NormalizedQuery, item: &Item, outcome: &MatchOutcome) -> Score;
}

/// User history contributions can be switched off entirely (spec 11.3).
#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryPolicy {
    pub enabled: bool,
}

/// The default ranker (spec 11.3).
///
/// Carries only the history policy; scoring is otherwise a pure function of
/// the signals handed to it, which is what makes tie-breaking reproducible
/// from one generation to the next (spec 11.6).
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultRanker {
    history: HistoryPolicy,
}

impl DefaultRanker {
    /// Builds a ranker that honours `history`.
    pub fn new(history: HistoryPolicy) -> Self {
        Self { history }
    }

    /// Combines a full signal set into an ordering key.
    ///
    /// Every signal contributes through a bounded, saturating curve, so the
    /// result is finite for any input, including `f32::MAX`, `u64::MAX`, NaN
    /// and the infinities, and identical inputs always produce an identical
    /// key.
    ///
    /// With [`HistoryPolicy::enabled`] cleared, `selection_frequency`,
    /// `selection_recency_secs`, and `query_history` contribute exactly zero
    /// and every other signal keeps the contribution it had.
    pub fn score_signals(&self, signals: RankingSignals) -> Score {
        let RankingSignals {
            match_quality,
            exact_prefix,
            match_position,
            category_weight,
            plugin_score_hint,
            selection_frequency,
            selection_recency_secs,
            query_history,
            context_match,
            user_preference,
        } = signals;

        // Textual and item signals.
        let mut total = W_MATCH_QUALITY * sanitize(match_quality, 0.0, 1.0);
        if exact_prefix {
            total += W_EXACT_PREFIX;
        }
        if let Some(match_position) = match_position {
            total += W_MATCH_POSITION * saturating_decay(match_position as f32, POSITION_HALF_LIFE);
        }
        total += W_CATEGORY * sanitize(category_weight, 0.0, 1.0);
        total += W_PLUGIN_HINT * bounded_signed(plugin_score_hint as f32, HINT_HALF_SCALE);

        // History signals. Disabling the policy skips the terms outright
        // rather than scaling them away, so an item with no history scores
        // identically under either policy (spec 11.3).
        if self.history.enabled {
            total += W_FREQUENCY * saturating_rise(selection_frequency as f32, FREQUENCY_HALF_LIFE);
            if let Some(age_secs) = selection_recency_secs {
                total += W_RECENCY * saturating_decay(age_secs as f32, RECENCY_HALF_LIFE_SECS);
            }
            total += W_QUERY_HISTORY * sanitize(query_history, 0.0, 1.0);
        }

        if context_match {
            total += W_CONTEXT;
        }
        total += W_USER_PREFERENCE * sanitize(user_preference, 0.0, 1.0);

        Score::new(sanitize(total, MIN_SCORE, MAX_SCORE))
    }

    /// Scores allocation-free match data with caller-provided dynamic signals.
    pub fn score_match_with_signals(
        &self,
        item: &Item,
        summary: MatchSummary,
        mut signals: RankingSignals,
    ) -> Score {
        signals.match_quality = summary.score;
        signals.exact_prefix = summary.method == MatchMethod::ExactPrefix;
        signals.match_position = summary.match_position;
        signals.category_weight = category_weight(&item.category);
        signals.plugin_score_hint = item.score_hint;
        self.score_signals(signals)
    }

    /// Scores a published outcome with caller-provided dynamic signals.
    pub fn score_outcome_with_signals(
        &self,
        item: &Item,
        outcome: &MatchOutcome,
        mut signals: RankingSignals,
    ) -> Score {
        signals.match_quality = outcome.score;
        signals.exact_prefix = outcome.method == MatchMethod::ExactPrefix;
        signals.match_position = earliest_highlight(&item.label, &outcome.highlights);
        signals.category_weight = category_weight(&item.category);
        signals.plugin_score_hint = item.score_hint;
        self.score_signals(signals)
    }

    /// Scores allocation-free match data produced during bounded selection.
    pub fn score_match(&self, item: &Item, summary: MatchSummary) -> Score {
        self.score_match_with_signals(item, summary, RankingSignals::default())
    }

    /// Highest score this item can earn without a label-prefix match.
    ///
    /// The bound maximizes every optional signal as well as the strongest
    /// non-prefix match. That keeps it valid for callers that use an enabled
    /// history policy or supply context and preference signals.
    ///
    /// `match_quality` is the ceiling of the strongest non-prefix band, which is
    /// [`MatchMethod::WordPrefix`]. Reordering the bands in `crikey-query`
    /// without revisiting this constant would prune candidates that can in fact
    /// outrank what has already been retained.
    pub fn non_prefix_upper_bound(&self, item: &Item) -> Score {
        self.score_signals(RankingSignals {
            match_quality: 0.73,
            exact_prefix: false,
            match_position: Some(0),
            category_weight: category_weight(&item.category),
            plugin_score_hint: item.score_hint,
            selection_frequency: u32::MAX,
            selection_recency_secs: Some(0),
            query_history: 1.0,
            context_match: true,
            user_preference: 1.0,
        })
    }
}

/// Maps a matched candidate onto [`RankingSignals`] and scores it.
///
/// Dynamic facts not carried by [`Item`] or [`MatchOutcome`] are left neutral:
/// selection frequency, selection recency, query-specific history, application
/// context, and configured user preference. Callers holding those facts should
/// build the signal set themselves and use [`DefaultRanker::score_signals`].
impl Ranker for DefaultRanker {
    fn score(&self, _query: &NormalizedQuery, item: &Item, outcome: &MatchOutcome) -> Score {
        self.score_signals(RankingSignals {
            match_quality: outcome.score,
            // `ExactPrefix` means the query reproduced the whole label. An
            // ordinary start-of-label `Prefix` match does not earn this bonus.
            exact_prefix: outcome.method == MatchMethod::ExactPrefix,
            match_position: earliest_highlight(&item.label, &outcome.highlights),
            category_weight: category_weight(&item.category),
            plugin_score_hint: item.score_hint,
            selection_frequency: 0,
            selection_recency_secs: None,
            query_history: 0.0,
            context_match: false,
            user_preference: 0.0,
        })
    }
}

// ---------------------------------------------------------------------------
// Signal extraction
// ---------------------------------------------------------------------------

/// Logical character offset of the earliest highlighted byte range.
///
/// Match outcomes expose raw-label byte ranges for rendering. Ranking converts
/// that boundary to a character position so equivalent Unicode labels receive
/// the same positional contribution. No highlight means no positional evidence
/// and therefore no position bonus. Malformed public ranges are ignored rather
/// than being treated as evidence at an arbitrary byte offset.
fn earliest_highlight(label: &str, highlights: &[(usize, usize)]) -> Option<u32> {
    let earliest_byte = highlights
        .iter()
        .filter_map(|&(start, end)| (start < end && label.get(start..end).is_some()).then_some(start))
        .min()?;
    let character_position = label
        .char_indices()
        .take_while(|&(byte_offset, _)| byte_offset < earliest_byte)
        .count();
    Some(u32::try_from(character_position).unwrap_or(u32::MAX))
}

/// Static category preference (spec 11.3).
///
/// A plugin-defined category sits at the neutral mid-point: the host has no
/// opinion about a category it has never heard of, and the owning plugin can
/// express its own through `Item::score_hint`.
fn category_weight(category: &Category) -> f32 {
    match category {
        Category::Application => 1.0,
        Category::Keyword => 0.9,
        Category::Command => 0.85,
        Category::Expression => 0.8,
        Category::Contact => 0.7,
        Category::Url => 0.65,
        Category::Directory => 0.6,
        Category::File => 0.55,
        Category::ClipboardItem => 0.5,
        Category::PluginDefined(_) => 0.5,
    }
}

// ---------------------------------------------------------------------------
// Bounded curves
//
// Each takes a non-negative finite input derived from an integer or an already
// sanitized float, and each is monotone, so composing them with non-negative
// weights keeps the whole score monotone. `half` is strictly positive, so the
// divisors below can never be zero.
// ---------------------------------------------------------------------------

/// Folds a hostile float onto `min..=max`.
///
/// [`f32::clamp`] propagates NaN, which is exactly the value that must not
/// reach the ordering key, so NaN is mapped to `min` first: an unusable signal
/// earns nothing.
fn sanitize(value: f32, min: f32, max: f32) -> f32 {
    if value.is_nan() {
        min
    } else {
        value.clamp(min, max)
    }
}

/// Saturating rise over `0.0..1.0`: `0.0` maps to exactly `0.0` and larger
/// inputs approach `1.0` without reaching it. `half` maps to `0.5`.
fn saturating_rise(value: f32, half: f32) -> f32 {
    value / (value + half)
}

/// Saturating decay over `0.0..=1.0`: `0.0` maps to exactly `1.0` and larger
/// inputs approach `0.0` without reaching it. `half` maps to `0.5`.
fn saturating_decay(value: f32, half: f32) -> f32 {
    half / (value + half)
}

/// Bounded odd curve over `-1.0..1.0`: `0.0` maps to exactly `0.0` and large
/// magnitudes approach `-1.0` and `1.0`. `half` maps to `0.5`, `-half` to `-0.5`.
///
/// Rational rather than `tanh`: divisions are correctly rounded, so the curve
/// is bit-identical everywhere and cannot reorder a result list across hosts.
fn bounded_signed(value: f32, half: f32) -> f32 {
    let scaled = value / half;
    scaled / (1.0 + scaled.abs())
}
