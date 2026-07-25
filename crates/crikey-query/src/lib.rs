//! Query engine primitives (spec 11).
//!
//! Normalization, tokenization and candidate matching all execute in Rust on
//! the hot path; plugins submit searchable data rather than reimplementing
//! matching.
//!
//! [`DefaultNormalizer`] applies NFKC-compatible full Unicode case folding,
//! then splits the result on Unicode whitespace (spec 11.1).
//!
//! [`DefaultMatcher`] scores an [`Item`] against those tokens. Every token must
//! match (logical AND); each token is credited with the strongest
//! interpretation it supports, in the fixed precedence
//! [`ExactPrefix`](MatchMethod::ExactPrefix) > [`Prefix`](MatchMethod::Prefix) >
//! [`Substring`](MatchMethod::Substring) > [`Acronym`](MatchMethod::Acronym) >
//! [`Keyword`](MatchMethod::Keyword) > [`Fuzzy`](MatchMethod::Fuzzy); and the
//! outcome as a whole is characterised by its *weakest* token so that one
//! strong token cannot inflate an otherwise poor candidate.
//!
//! Highlights are byte ranges into the **raw** label. Normalization can change
//! byte lengths (`ﬁ` folds to `fi`, `Ⅷ` folds to `viii`), so the matcher keeps a
//! mapping from normalized offsets back to source offsets instead of reusing
//! normalized offsets directly, which would slice labels mid-character.

use std::borrow::Cow;

use crikey_core::Item;
use unicode_casefold::UnicodeCaseFold;
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

/// A normalized query ready for matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedQuery {
    /// The query exactly as typed, preserved verbatim for echoing and history.
    pub raw: String,
    /// `raw` after NFKC-compatible full Unicode case folding. Whitespace is
    /// kept so that `normalized.split_whitespace()` reproduces [`tokens`](Self::tokens).
    pub normalized: String,
    /// `normalized` split on Unicode whitespace. Never contains empty tokens.
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

impl MatchMethod {
    /// Precedence rank of this method; **lower is stronger**.
    ///
    /// Declaration order of the variants is deliberately *not* the precedence
    /// order, so consumers that need to compare textual match quality must go
    /// through this method rather than deriving an ordering from the enum.
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            Self::ExactPrefix => 0,
            Self::Prefix => 1,
            Self::Substring => 2,
            Self::Acronym => 3,
            Self::Keyword => 4,
            Self::Fuzzy => 5,
        }
    }
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

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/// The default [`Normalizer`]: NFKC-compatible full Unicode case folding,
/// followed by whitespace tokenization.
///
/// Full, locale-independent case folding makes caseless equivalents such as
/// Greek sigma forms and `ß`/`ss` agree. A final NFKC pass composes any
/// sequences introduced by the fold.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultNormalizer {
    _private: (),
}

impl Normalizer for DefaultNormalizer {
    fn normalize(&self, raw: &str) -> NormalizedQuery {
        let normalized = normalize_text(raw);
        let tokens = normalized.split_whitespace().map(str::to_owned).collect();
        NormalizedQuery {
            raw: raw.to_owned(),
            normalized,
            tokens,
        }
    }
}

/// Appends the NFKC-compatible full Unicode caseless form of `raw` to `out`.
fn push_normalized(raw: &str, out: &mut String) {
    out.extend(raw.nfkc().case_fold().nfkc());
}

fn normalize_text(raw: &str) -> String {
    // NFKC never expands beyond a small constant factor for realistic text, so
    // the source length is a reasonable starting capacity.
    let mut out = String::with_capacity(raw.len());
    push_normalized(raw, &mut out);
    out
}

/// Normalizes an item field, borrowing when the source is already normalized.
///
/// ASCII is fixed by NFKC and lowercases byte-for-byte, so the common case
/// costs nothing beyond the scan.
fn normalize_field(raw: &str) -> Cow<'_, str> {
    if raw.is_ascii() {
        if raw.bytes().any(|byte| byte.is_ascii_uppercase()) {
            Cow::Owned(raw.to_ascii_lowercase())
        } else {
            Cow::Borrowed(raw)
        }
    } else {
        Cow::Owned(normalize_text(raw))
    }
}

// ---------------------------------------------------------------------------
// Normalized label with a mapping back to raw byte offsets
// ---------------------------------------------------------------------------

/// One normalized character and the raw byte range it was folded from.
#[derive(Debug, Clone, Copy)]
struct Mark {
    /// Byte offset of the character inside the normalized text.
    norm: u32,
    src_start: u32,
    src_end: u32,
}

/// Raw bytes an unresolved normalization run may span before the remaining
/// normalized tail is marked unmappable.
///
/// Resolving a run costs one re-normalization per character, so an unbounded
/// run would be quadratic on a hostile label built from a single starter and a
/// storm of combining marks. Unicode's own stream-safe format caps a combining
/// sequence at 30 marks, well inside this budget.
const MAX_RUN_BYTES: usize = 128;

/// How normalized byte offsets map back onto raw label bytes.
#[derive(Debug)]
enum OffsetMap {
    /// The label is ASCII: normalization is byte-for-byte, offsets are equal.
    Identity,
    /// Precisely mapped normalized prefix. Bytes at or beyond `mapped_end`
    /// belong to a degraded tail and must not produce highlights.
    Marks { marks: Vec<Mark>, mapped_end: u32 },
    /// The label is too large to address; highlights are suppressed.
    Unavailable,
}

/// An item label folded once per match, plus the map back to raw offsets.
#[derive(Debug)]
struct NormalizedLabel<'a> {
    text: Cow<'a, str>,
    map: OffsetMap,
    /// Character count of `text`, used for coverage ratios.
    char_len: usize,
}

impl<'a> NormalizedLabel<'a> {
    fn new(raw: &'a str) -> Self {
        if raw.is_ascii() {
            let text = normalize_field(raw);
            let char_len = text.len();
            return Self {
                text,
                map: OffsetMap::Identity,
                char_len,
            };
        }

        let text = normalize_text(raw);
        let char_len = text.chars().count();
        let map = if u32::try_from(raw.len()).is_ok() && u32::try_from(text.len()).is_ok() {
            let (marks, mapped_end) = align_marks(raw, &text, char_len);
            OffsetMap::Marks { marks, mapped_end }
        } else {
            OffsetMap::Unavailable
        };
        Self {
            text: Cow::Owned(text),
            map,
            char_len,
        }
    }

    /// Translates a byte range of the normalized label into the raw label byte
    /// range it came from.
    ///
    /// Returns `None` for an empty range or when no mapping is available. Every
    /// returned bound is a character boundary of the raw label, because it is
    /// copied from a boundary observed while walking that label.
    fn to_raw(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        if start >= end {
            return None;
        }
        match &self.map {
            OffsetMap::Identity => Some((start, end)),
            OffsetMap::Unavailable => None,
            OffsetMap::Marks { marks, mapped_end } => {
                if end > *mapped_end as usize {
                    return None;
                }
                let first = marks.partition_point(|mark| (mark.norm as usize) < start);
                let past = marks.partition_point(|mark| (mark.norm as usize) < end);
                if first >= past {
                    return None;
                }
                let from = marks.get(first)?;
                let to = marks.get(past - 1)?;
                (from.src_start < to.src_end).then_some((from.src_start as usize, to.src_end as usize))
            }
        }
    }
}

/// Aligns the precisely attributable prefix of `text` (the normalization of
/// `raw`) back onto `raw`, one mark per normalized character.
///
/// Characters are consumed one at a time and the accumulated run is normalized
/// and tested against the authoritative `text`. A run that does not yet line up
/// is still open — this is what makes composition (`e` + `U+0301` -> `é`),
/// expansion (`ﬁ` -> `fi`), canonical reordering and Hangul jamo composition
/// fall out of a single rule instead of a table of special cases. If a run
/// exceeds the work budget, the remaining normalized tail stays unmapped.
fn align_marks(raw: &str, text: &str, char_len: usize) -> (Vec<Mark>, u32) {
    let mut marks = Vec::with_capacity(char_len);
    let mut scratch = String::new();
    // Byte offset in `raw` where the currently open run starts.
    let mut run_start = 0usize;
    // Byte offset in `text` of the first character not yet attributed.
    let mut cursor = 0usize;

    for (index, ch) in raw.char_indices() {
        let run_end = index + ch.len_utf8();
        scratch.clear();
        push_normalized(&raw[run_start..run_end], &mut scratch);

        if text[cursor..].starts_with(scratch.as_str()) {
            for (offset, _) in scratch.char_indices() {
                marks.push(Mark {
                    norm: (cursor + offset) as u32,
                    src_start: run_start as u32,
                    src_end: run_end as u32,
                });
            }
            cursor += scratch.len();
            run_start = run_end;
        } else if run_end - run_start >= MAX_RUN_BYTES {
            break;
        }
    }

    (marks, cursor as u32)
}

// ---------------------------------------------------------------------------
// Scoring bands
// ---------------------------------------------------------------------------

/// Score band for a method: `(floor, ceiling)`.
///
/// The bands are disjoint and ordered by [`MatchMethod::precedence`], so the
/// reported method alone fixes the coarse rank while the within-band quality
/// term separates candidates that matched the same way. Every band sits inside
/// `(0.0, 1.0]`.
const fn band(method: MatchMethod) -> (f32, f32) {
    match method {
        MatchMethod::ExactPrefix => (0.90, 1.00),
        MatchMethod::Prefix => (0.75, 0.88),
        MatchMethod::Substring => (0.58, 0.72),
        MatchMethod::Acronym => (0.42, 0.55),
        MatchMethod::Keyword => (0.26, 0.39),
        MatchMethod::Fuzzy => (0.10, 0.23),
    }
}

/// Clamps a quality term into `[0.0, 1.0]`, mapping non-finite input to zero.
fn unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Division that yields `0.0` instead of a non-finite value on a zero divisor.
fn ratio(numerator: usize, denominator: usize) -> f32 {
    if denominator == 0 {
        return 0.0;
    }
    unit(numerator as f32 / denominator as f32)
}

fn score_for(method: MatchMethod, quality: f32) -> f32 {
    let (low, high) = band(method);
    (low + (high - low) * unit(quality)).clamp(low, high)
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// The default [`Matcher`] (spec 11.1).
///
/// Prefix, substring, acronym and fuzzy matching run against the label, which
/// is the only field highlights can point at. Keyword matching additionally
/// covers the fields a plugin submits for search — `search_terms` and
/// `description` — by containment only. The `target` is deliberately excluded:
/// it is an execution payload (a path, URL or command line), and matching it
/// would make every `/usr/bin/...` item answer to `usr`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultMatcher {
    _private: (),
}

impl Matcher for DefaultMatcher {
    fn match_item(&self, query: &NormalizedQuery, item: &Item) -> Option<MatchOutcome> {
        if query.tokens.is_empty()
            || !query
                .normalized
                .split_whitespace()
                .eq(query.tokens.iter().map(String::as_str))
        {
            return None;
        }

        let label = NormalizedLabel::new(&item.label);
        if let Some(outcome) = exact_label_match(query, &label) {
            return Some(outcome);
        }

        let mut view = ItemView {
            label,
            keywords: None,
            item,
        };
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut weakest = MatchMethod::ExactPrefix;
        let mut quality_total = 0.0f32;

        for token in &query.tokens {
            let (method, quality) = match_token(&mut view, token, &mut spans)?;
            if method.precedence() > weakest.precedence() {
                weakest = method;
            }
            quality_total += unit(quality);
        }

        // `tokens` is non-empty, so the mean is well defined.
        let quality = quality_total / query.tokens.len() as f32;
        Some(MatchOutcome {
            score: score_for(weakest, quality),
            method: weakest,
            highlights: merge_spans(spans),
        })
    }
}

/// Per-item state shared by every token of one query.
///
/// The label is folded once; the keyword fields are folded lazily, so a query
/// whose tokens all land on the label never pays for them.
#[derive(Debug)]
struct ItemView<'a> {
    label: NormalizedLabel<'a>,
    keywords: Option<Vec<Cow<'a, str>>>,
    item: &'a Item,
}

impl<'a> ItemView<'a> {
    fn keywords(&mut self) -> &[Cow<'a, str>] {
        let item = self.item;
        self.keywords
            .get_or_insert_with(|| {
                let mut fields = Vec::with_capacity(item.search_terms.len() + 1);
                fields.extend(item.search_terms.iter().map(|term| normalize_field(term)));
                if !item.description.is_empty() {
                    fields.push(normalize_field(&item.description));
                }
                fields
            })
            .as_slice()
    }
}

/// The whole query reproduces the whole label: the strongest possible match.
fn exact_label_match(query: &NormalizedQuery, label: &NormalizedLabel<'_>) -> Option<MatchOutcome> {
    let trimmed = label.text.trim();
    if trimmed.is_empty() || query.normalized.trim() != trimmed {
        return None;
    }
    let start = label.text.len() - label.text.trim_start().len();
    Some(MatchOutcome {
        score: score_for(MatchMethod::ExactPrefix, 1.0),
        method: MatchMethod::ExactPrefix,
        highlights: label.to_raw(start, start + trimmed.len()).into_iter().collect(),
    })
}

/// Strongest interpretation of a single token, appending its label highlights.
fn match_token(
    view: &mut ItemView<'_>,
    token: &str,
    spans: &mut Vec<(usize, usize)>,
) -> Option<(MatchMethod, f32)> {
    // The normalizer never emits one, but `NormalizedQuery` is constructible by
    // hand: an empty token carries no evidence, so it cannot license a match.
    if token.is_empty() {
        return None;
    }

    let token_chars = token.chars().count();

    if let Some(found) = match_label(&view.label, token, token_chars, spans) {
        return Some(found);
    }

    let keyword = keyword_quality(view.keywords(), token, token_chars);
    if let Some(quality) = keyword {
        return Some((MatchMethod::Keyword, quality));
    }

    fuzzy_quality(&view.label, token, token_chars, spans).map(|quality| (MatchMethod::Fuzzy, quality))
}

/// Prefix, substring and acronym matching, in precedence order.
fn match_label(
    label: &NormalizedLabel<'_>,
    token: &str,
    token_chars: usize,
    spans: &mut Vec<(usize, usize)>,
) -> Option<(MatchMethod, f32)> {
    if label.text.starts_with(token) {
        push_span(spans, label.to_raw(0, token.len()));
        return Some((MatchMethod::Prefix, ratio(token_chars, label.char_len)));
    }

    if let Some(at) = label.text.find(token) {
        push_span(spans, label.to_raw(at, at + token.len()));
        let coverage = ratio(token_chars, label.char_len);
        // A hit close to the front of the label reads as more relevant.
        let position = 1.0 / (1 + label.text[..at].chars().count()) as f32;
        return Some((MatchMethod::Substring, 0.5 * coverage + 0.5 * position));
    }

    acronym_quality(label, token, token_chars, spans).map(|quality| (MatchMethod::Acronym, quality))
}

/// The token spells the leading word initials of the label.
fn acronym_quality(
    label: &NormalizedLabel<'_>,
    token: &str,
    token_chars: usize,
    spans: &mut Vec<(usize, usize)>,
) -> Option<f32> {
    // A single letter is a prefix, not an acronym.
    if token_chars < 2 {
        return None;
    }

    let mark = spans.len();
    let mut initials = word_initials(&label.text);
    let mut matched = 0usize;

    for needle in token.chars() {
        match initials.next() {
            Some((index, initial)) if initial == needle => {
                push_span(spans, label.to_raw(index, index + initial.len_utf8()));
                matched += 1;
            }
            _ => {
                spans.truncate(mark);
                return None;
            }
        }
    }

    // Covering every word is a better acronym than covering only the first few.
    Some(ratio(matched, matched + initials.count()))
}

/// The token's characters occur in the label in order, not necessarily adjacent.
fn fuzzy_quality(
    label: &NormalizedLabel<'_>,
    token: &str,
    token_chars: usize,
    spans: &mut Vec<(usize, usize)>,
) -> Option<f32> {
    // A single character is a substring, not a fuzzy match.
    if token_chars < 2 {
        return None;
    }

    let mark = spans.len();
    let mut haystack = label.text.char_indices().enumerate();
    let mut first_ordinal = 0usize;
    let mut last_ordinal = 0usize;
    let mut matched = 0usize;

    for needle in token.chars() {
        let hit = loop {
            match haystack.next() {
                Some((ordinal, (index, ch))) if ch == needle => break Some((ordinal, index, ch)),
                Some(_) => {}
                None => break None,
            }
        };
        let Some((ordinal, index, ch)) = hit else {
            spans.truncate(mark);
            return None;
        };
        let width = ch.len_utf8();
        if matched == 0 {
            first_ordinal = ordinal;
        }
        last_ordinal = ordinal;
        matched += 1;
        push_span(spans, label.to_raw(index, index + width));
    }

    // Tight, early runs read as intentional; scattered ones as coincidence.
    let compactness = ratio(matched, last_ordinal - first_ordinal + 1);
    let earliness = 1.0 / (1 + first_ordinal) as f32;
    Some(0.5 * compactness + 0.5 * earliness)
}

/// The token is contained in one of the plugin-supplied searchable fields.
fn keyword_quality(fields: &[Cow<'_, str>], token: &str, token_chars: usize) -> Option<f32> {
    let mut best = 0.0f32;
    let mut found = false;

    for field in fields {
        let Some(at) = field.find(token) else { continue };
        let coverage = ratio(token_chars, field.chars().count());
        let quality = if at > 0 {
            0.5 * coverage
        } else if token.len() == field.len() {
            1.0
        } else {
            0.5 + 0.5 * coverage
        };
        found = true;
        if quality > best {
            best = quality;
        }
    }

    found.then_some(best)
}

/// Byte offset and character of the first character of each word.
///
/// Words are runs of alphanumeric characters plus Unicode marks, so combining
/// marks cannot invent boundaries inside a word. Other punctuation separates
/// words as before.
fn word_initials(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut inside_word = false;
    text.char_indices().filter_map(move |(index, ch)| {
        if is_mark(ch) {
            return None;
        }
        if !ch.is_alphanumeric() {
            inside_word = false;
            return None;
        }
        if inside_word {
            return None;
        }
        inside_word = true;
        Some((index, ch))
    })
}

fn is_mark(ch: char) -> bool {
    !ch.is_ascii()
        && matches!(
            get_general_category(ch),
            GeneralCategory::NonspacingMark | GeneralCategory::SpacingMark | GeneralCategory::EnclosingMark
        )
}

fn push_span(spans: &mut Vec<(usize, usize)>, span: Option<(usize, usize)>) {
    if let Some(span) = span {
        spans.push(span);
    }
}

/// Sorts highlights and coalesces overlapping or touching ranges in place, so
/// the outcome is always ordered, disjoint and non-empty range by range.
fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if spans.len() < 2 {
        return spans;
    }
    spans.sort_unstable();
    spans.dedup_by(|current, previous| {
        if current.0 > previous.1 {
            return false;
        }
        if current.1 > previous.1 {
            previous.1 = current.1;
        }
        true
    });
    spans
}
