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
//! [`WordPrefix`](MatchMethod::WordPrefix) >
//! [`Substring`](MatchMethod::Substring) > [`Keyword`](MatchMethod::Keyword);
//! and the outcome as a whole is characterised by its *weakest* token so that
//! one strong token cannot inflate an otherwise poor candidate.
//!
//! [`WordPrefix`](MatchMethod::WordPrefix) is what makes abbreviations work:
//! the token is split into chunks that are each a prefix of a distinct label
//! word, taken left to right, so `vscode` reads as `v|s|code` over
//! `Visual Studio Code`. Requiring every chunk to start a word is what keeps
//! `manic` away from `Memory Diagnostic Tool` — there is no such split — while
//! still admitting initialisms, which are the special case where every chunk is
//! one character long.
//!
//! Highlights are byte ranges into the **raw** label. Normalization can change
//! byte lengths (`ﬁ` folds to `fi`, `Ⅷ` folds to `viii`), so the matcher keeps a
//! mapping from normalized offsets back to source offsets instead of reusing
//! normalized offsets directly, which would slice labels mid-character.
//!
//! [`presence_mask`] is the cheap half of that work: a 64-bit set over the
//! normalized characters of a text. Because every match method needs each
//! query character to occur in the item, an item whose mask is missing one of
//! them cannot match, and the catalog can skip scoring it entirely
//! (spec 11.1). [`searchable_text`] folds the fields the mask is taken over
//! once, at index time.

use std::{borrow::Cow, collections::BTreeMap};

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
    /// The token's characters occur in the label in order but not adjacently,
    /// and not aligned to word boundaries (spec 11.1 fuzzy matching).
    ///
    /// Only reachable under [`MatchPolicy::Subsequence`]. It is the weakest and
    /// least selective reading available: `manic` matches `Memory Diagnostic
    /// Tool` this way, which is why no default configuration admits it.
    Fuzzy,
    /// The token splits into two or more chunks that are each a prefix of a
    /// distinct label word, consumed left to right. Subsumes initialisms (`tm`
    /// for `Task Manager`) and mixed abbreviations (`vscode` for
    /// `Visual Studio Code`).
    WordPrefix,
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
            Self::WordPrefix => 2,
            Self::Substring => 3,
            Self::Keyword => 4,
            Self::Fuzzy => 5,
        }
    }
}

/// Which readings of a token a matcher is allowed to credit.
///
/// Ordered-subsequence matching is opt-in because it cannot be made selective:
/// the query `manic` and the query `vscode` produce indistinguishable evidence
/// against `Memory Diagnostic Tool` and `Visual Studio Code` respectively, so no
/// threshold separates the coincidence from the abbreviation. Word-prefix
/// matching handles the abbreviation, and this policy exists for callers that
/// deliberately want the looser behaviour as well.
///
/// The policy is also a candidate-pruning contract. A caller narrowing a
/// previously accepted candidate set must have built that set under the *same*
/// policy, or a strict pass will have discarded the very candidates a
/// subsequence pass needs. [`PreparedLabel::may_match_with`] takes the policy
/// for exactly that reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MatchPolicy {
    /// Every reading except ordered subsequence. The default everywhere.
    #[default]
    Strict,
    /// Additionally credits ordered-subsequence matches as
    /// [`MatchMethod::Fuzzy`].
    Subsequence,
}

/// Score plus the highlight ranges that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchOutcome {
    pub score: f32,
    pub method: MatchMethod,
    /// Byte ranges within the item label, for UI highlighting.
    pub highlights: Vec<(usize, usize)>,
}

/// Allocation-free match data sufficient for ranking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchSummary {
    pub score: f32,
    pub method: MatchMethod,
    /// Character offset of the earliest label match.
    pub match_position: Option<u32>,
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

/// User-defined query aliases, applied to a normalized query before matching.
///
/// An alias is a *rewrite*, not an extra reading: the token `vsc` becomes the
/// tokens of `Visual Studio Code`, and the search that follows is an ordinary
/// search for those tokens. That is the whole reason this lives here rather
/// than in the catalog. Every prefilter downstream - the presence mask, the
/// ordered-pair postings, the prefix postings, `may_match` - derives from the
/// query, so a rewritten query is automatically sound for all of them. Tagging
/// items with alias terms instead would put host configuration inside plugin
/// data: it would be persisted into the catalog cache, counted against the
/// publishing plugin's payload limits, and would have to be unpicked from
/// `search_terms` by position on every reload.
///
/// The cost of rewriting is that the literal reading of an aliased token is
/// not also tried. A user who defines `ss` has said what `ss` means to them;
/// an item whose own text reads `ss` is no longer found by it. Aliases are
/// opt-in per entry, so nothing is lost that was not asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliasTable {
    /// Folded single-token alias to the folded tokens it stands for.
    entries: BTreeMap<String, Vec<String>>,
}

impl AliasTable {
    /// Builds a table from `alias -> target` text pairs, folding both sides
    /// with the same rules the matcher uses so that what a user typed and what
    /// they configured compare as equals.
    ///
    /// Entries that could never fire are dropped rather than stored:
    ///
    /// * an alias that folds to nothing, or to more than one token, can never
    ///   equal a single query token;
    /// * a target that folds to nothing would delete the token instead of
    ///   replacing it, widening the query to match more than the user typed.
    pub fn new<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let entries = pairs
            .into_iter()
            .filter_map(|(alias, target)| {
                let alias = normalize_text(alias.as_ref());
                let mut names = alias.split_whitespace();
                let name = names.next()?.to_owned();
                if names.next().is_some() {
                    return None;
                }
                let target = normalize_text(target.as_ref());
                let tokens: Vec<String> = target.split_whitespace().map(str::to_owned).collect();
                (!tokens.is_empty()).then_some((name, tokens))
            })
            .collect();
        Self { entries }
    }

    /// Whether any alias is defined. An empty table rewrites nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many aliases are defined.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Rewrites every token that names an alias, leaving the rest alone.
    ///
    /// Expansion is single pass: the tokens an alias produces are never
    /// themselves expanded. That makes `a = "b"`, `b = "a"` terminate with no
    /// cycle check, and means an alias always stands for what it literally
    /// says rather than for whatever another entry later redefines.
    ///
    /// `raw` is untouched, so the UI still echoes what the user typed.
    #[must_use]
    pub fn expand(&self, query: NormalizedQuery) -> NormalizedQuery {
        if self.entries.is_empty() || query.tokens.is_empty() {
            return query;
        }
        if !query.tokens.iter().any(|token| self.entries.contains_key(token)) {
            return query;
        }

        let mut tokens = Vec::with_capacity(query.tokens.len());
        for token in &query.tokens {
            match self.entries.get(token) {
                Some(expansion) => tokens.extend(expansion.iter().cloned()),
                None => tokens.push(token.clone()),
            }
        }
        // Rebuilt rather than edited: `normalized.split_whitespace()` must keep
        // reproducing `tokens`, which is what every later reader relies on.
        NormalizedQuery {
            raw: query.raw,
            normalized: tokens.join(" "),
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
// Presence masks (candidate pruning)
// ---------------------------------------------------------------------------

/// Bits standing for exactly one character: `a`-`z` in `0..26`, `0`-`9` in
/// `26..36`. These are the characters queries are actually made of, so giving
/// them private bits is what makes the filter selective.
pub const DEDICATED_BITS: u32 = 36;

/// Bits every other character shares.
const SHARED_BITS: u32 = u64::BITS - DEDICATED_BITS;

/// Spreads a code point across the shared buckets.
///
/// Accented Latin, Greek, Cyrillic and CJK all arrive as runs of adjacent code
/// points, so a plain remainder would drop a whole script into two or three
/// buckets and blunt the filter. This is the usual xor-shift-multiply
/// avalanche: fixed constants, integer arithmetic only, therefore identical on
/// every platform and every run — a mask stored at index time still means the
/// same thing when a query is folded against it later.
const fn scramble(code: u32) -> u32 {
    let mut hash = code ^ 0x9e37_79b9;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^ (hash >> 16)
}

/// The one bit standing for an already-normalized character, or `0` for
/// whitespace.
///
/// Whitespace is what [`Normalizer`] splits tokens *on*, so no query token can
/// contain any. Dropping it costs nothing on the query side and stops a
/// character carried by practically every item from occupying — and so
/// permanently satisfying — one of the scarce shared buckets.
fn char_bit(ch: char) -> u64 {
    let code = ch as u32;
    let bit = match ch {
        'a'..='z' => code - 'a' as u32,
        '0'..='9' => code - '0' as u32 + 26,
        _ if ch.is_whitespace() => return 0,
        _ => DEDICATED_BITS + scramble(code) % SHARED_BITS,
    };
    1u64 << bit
}

/// A 64-bit set over the distinct **normalized** characters of `text`.
///
/// This is the catalog's candidate prefilter (spec 11.1). Every method
/// [`DefaultMatcher`] supports — exact prefix, prefix, word prefix, substring
/// and keyword — can only fire when every character of the token already occurs
/// somewhere in the item's searchable text, in order. So for a token mask `q`
/// and an item mask `m`, `q & !m == 0` is a *necessary* condition for a match,
/// and an item it rejects cannot have matched.
///
/// This invariant is why the matcher admits no edit-distance tier: a typo
/// substitutes or transposes characters, so a token that is *close to* an item
/// need not be a subsequence of it, and the prefilter would prune the candidate
/// before scoring. Adding typo tolerance means adding an index that tolerates
/// typos, not just a scoring method.
///
/// # Invariant
///
/// The mask is a monotone union over characters: a superset of characters sets
/// a superset of bits, and no character ever clears one. Two things follow,
/// and both are the safe direction. Adding text to an item — a description, a
/// search term — can only admit it for *more* queries, never fewer. And two
/// characters sharing a bucket merely make the filter less discriminating, so
/// a collision can only let an item through to the matcher, which then answers
/// exactly as it would have without any pruning. There are no false negatives
/// by construction.
///
/// The input is folded with the crate's own normalization, so `Chapter Ⅷ`
/// answers to `viii`, `Straße` to `strasse` and `ﬁle` to `file`. Folding is
/// idempotent, so already-normalized text — an item's cached searchable text,
/// a [`NormalizedQuery`] token — costs only the scan and gives the same
/// answer. Neither path builds an intermediate string.
pub fn presence_mask(text: &str) -> u64 {
    if text.is_ascii() {
        // ASCII is fixed by NFKC and folds by lowercasing, so the byte is the
        // character.
        text.bytes()
            .fold(0, |mask, byte| mask | char_bit(byte.to_ascii_lowercase() as char))
    } else {
        text.nfkc()
            .case_fold()
            .nfkc()
            .fold(0, |mask, ch| mask | char_bit(ch))
    }
}

/// Everything the matcher is allowed to read from `item`, folded once: the
/// label, the description and the search terms, joined by single spaces.
///
/// Intended to be computed once when an item enters the catalog and kept
/// beside it, so a query pays for [`presence_mask`] over one already-folded
/// string instead of re-folding every field on every keystroke.
///
/// The `target` and the `stable_id` are deliberately absent. The matcher
/// ignores both — a `target` is an execution payload, and folding it in would
/// offer candidates for `usr` that can never match.
///
/// This text is for masking only. Its byte offsets mean nothing in the raw
/// label, which is why [`DefaultMatcher`] folds the label separately and keeps
/// its own map back to raw offsets; highlight ranges must never be taken from
/// here.
pub fn searchable_text(item: &Item) -> String {
    searchable_text_with_label(item).0
}

/// [`searchable_text`], plus the byte length of the folded label prefix.
///
/// A catalog caching this text needs the label fold kept apart from the
/// keyword fold, so that whatever it does with a label never depends on where
/// a description happens to start. `text[..label_bytes]` is the folded label
/// and the rest is the folded keyword text, its leading separator included —
/// one allocation for both instead of a second pass over the item.
///
/// These are offsets into *folded* text. Folding changes byte lengths, so they
/// are not offsets into the raw label and must never be reported as
/// highlights; [`DefaultMatcher`] keeps its own map back to raw label bytes
/// for that.
pub fn searchable_text_with_label(item: &Item) -> (String, usize) {
    let terms = item.search_terms.iter().fold(0usize, |total, term| {
        total.saturating_add(term.len().saturating_add(1))
    });
    let capacity = item
        .label
        .len()
        .saturating_add(item.description.len())
        .saturating_add(terms)
        .saturating_add(1);
    let mut out = String::with_capacity(capacity);
    push_searchable_field(&mut out, &item.label);
    let label_bytes = out.len();
    push_searchable_field(&mut out, &item.description);
    for term in &item.search_terms {
        push_searchable_field(&mut out, term);
    }
    (out, label_bytes)
}

/// Appends one normalized field, separated from what came before by a space.
///
/// A space is a starter that nothing composes across, so folding field by
/// field agrees with folding the joined text.
fn push_searchable_field(out: &mut String, raw: &str) {
    if raw.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    if raw.is_ascii() {
        // Borrows outright when the field is already lowercase.
        out.push_str(&normalize_field(raw));
    } else {
        push_normalized(raw, out);
    }
}

// ---------------------------------------------------------------------------
// Normalized label with a mapping back to raw byte offsets
// ---------------------------------------------------------------------------

/// One normalized character and the raw byte range it was folded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
enum OffsetMap {
    /// Precisely mapped normalized prefix. Bytes at or beyond `mapped_end`
    /// belong to a degraded tail and must not produce highlights.
    Marks { marks: Vec<Mark>, mapped_end: u32 },
    /// The label is too large to address; highlights are suppressed.
    Unavailable,
}

/// An item label normalized once, with a map back to raw byte offsets.
///
/// Catalogs retain this beside the item so repeated queries do not normalize
/// the same label on every keystroke.
///
/// Two prepared labels are equal when every derived buffer agrees, which is
/// what lets [`DefaultMatcher::match_prepared`] prove a caller-supplied buffer
/// belongs to the item it is about to score. Folded equality alone would not:
/// `words` and `map` come from the raw label, and `PowerShell` and
/// `Powershell` fold to the same text while carrying different boundaries.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedLabel {
    /// Folded label followed by optional folded keyword text.
    text: Box<str>,
    label_bytes: usize,
    /// `None` is the byte-for-byte ASCII identity map.
    map: Option<Box<OffsetMap>>,
    /// Character count of the label portion of `text`.
    char_len: usize,
    /// Byte ranges of the label's words inside the **normalized** text,
    /// ascending and disjoint.
    ///
    /// Computed from the raw label because case folding destroys the camel-case
    /// boundaries word-prefix matching depends on: `PowerShell` folds to
    /// `powershell`, and nothing in the folded form recovers the split. Stored
    /// with one spare slot beyond [`MAX_WORD_PREFIX_WORDS`] so an over-cap label
    /// is detectable without retaining an unbounded list.
    words: Box<[(u32, u32)]>,
}

impl PreparedLabel {
    /// Normalizes `raw` and prepares highlight offset translation.
    pub fn new(raw: &str) -> Self {
        let text = normalize_field(raw).into_owned();
        let label_bytes = text.len();
        Self::from_searchable_text(raw, text, label_bytes)
    }

    /// Builds a prepared label from a folded searchable buffer.
    ///
    /// `label_bytes` is the folded label prefix length returned by
    /// [`searchable_text_with_label`]. If a caller supplies an offset that
    /// does not land on a UTF-8 boundary, the constructor safely falls back
    /// to preparing `raw` on its own.
    pub fn from_searchable_text(raw: &str, text: String, label_bytes: usize) -> Self {
        let Some(label) = text.get(..label_bytes) else {
            // This is a public performance-oriented constructor, so malformed
            // caller metadata must degrade to the safe standalone preparation
            // rather than panic while slicing a UTF-8 string.
            return Self::new(raw);
        };
        let char_len = label.chars().count();
        let map = if raw.is_ascii() {
            None
        } else if u32::try_from(raw.len()).is_ok() && u32::try_from(label.len()).is_ok() {
            let (marks, mapped_end) = align_marks(raw, label, char_len);
            Some(Box::new(OffsetMap::Marks { marks, mapped_end }))
        } else {
            Some(Box::new(OffsetMap::Unavailable))
        };
        let words = word_ranges(raw, label, map.as_deref());
        Self {
            text: text.into_boxed_str(),
            label_bytes,
            map,
            char_len,
            words,
        }
    }

    /// The normalized label used by the matcher.
    pub fn normalized(&self) -> &str {
        &self.text[..self.label_bytes]
    }

    /// Folded description and search terms retained after the label.
    pub fn keywords(&self) -> &str {
        self.text[self.label_bytes..]
            .strip_prefix(' ')
            .unwrap_or(&self.text[self.label_bytes..])
    }

    /// Every folded searchable field in catalog order.
    pub fn searchable_text(&self) -> &str {
        &self.text
    }

    /// Cheaply rejects items that no matcher interpretation can accept, under
    /// the default [`MatchPolicy::Strict`].
    ///
    /// This repeats only the boolean shape of matching over ingestion-time
    /// folded text: no highlight vectors, keyword normalization, outcomes or
    /// scores are built. Returning `true` is permission to run the full
    /// matcher; returning `false` is definitive.
    ///
    /// A candidate set narrowed with this method must not be handed to a
    /// [`MatchPolicy::Subsequence`] pass: strict pruning discards exactly the
    /// loose candidates such a pass exists to find. Use
    /// [`Self::may_match_with`] with the policy the matcher will run under.
    pub fn may_match(&self, query: &NormalizedQuery) -> bool {
        self.may_match_with(query, MatchPolicy::Strict)
    }

    /// [`Self::may_match`] under an explicit policy.
    ///
    /// The policy must be the one the matcher will run under, so that the
    /// admitted set stays a superset of the matched set across every keystroke.
    pub fn may_match_with(&self, query: &NormalizedQuery, policy: MatchPolicy) -> bool {
        if query.tokens.is_empty()
            || !query
                .normalized
                .split_whitespace()
                .eq(query.tokens.iter().map(String::as_str))
        {
            return false;
        }

        let label = self.normalized();
        let keywords = self.keywords();
        query
            .tokens
            .iter()
            .all(|token| self.token_may_match(label, keywords, token, policy))
    }

    /// The label's word ranges inside the normalized text.
    fn word_ranges(&self) -> &[(u32, u32)] {
        &self.words
    }

    /// Boolean counterpart of `match_token`: the disjunction over every tier the
    /// matcher can report under `policy`, evaluated over ingestion-time folded
    /// text.
    ///
    /// Each disjunct is prefix-closed — extending the token can only turn a
    /// `true` into a `false`, never the reverse — which is what lets a caller
    /// narrow a previously accepted candidate set as the user keeps typing.
    fn token_may_match(&self, label: &str, keywords: &str, token: &str, policy: MatchPolicy) -> bool {
        if token.is_empty() {
            return false;
        }
        // Prefix, substring and keyword.
        if label.contains(token) || keywords.contains(token) {
            return true;
        }
        // Both remaining readings consume the token's characters in order, so
        // one scan settles the shared precondition.
        if !is_ordered_subsequence(label, token) {
            return false;
        }
        // Word prefix, admitted through a necessary condition rather than the
        // decomposition itself. This runs for every candidate the catalog
        // revisits on every keystroke, so it must not allocate or run the grid.
        //
        // The subsequence precondition above is load bearing: `may_decompose`
        // inspects only the first two characters, so on its own it would admit
        // `manic` for `Task Manager` — a label containing neither `i` nor `c` —
        // and the candidate set would stop shrinking as the user types.
        if may_decompose(self.word_ranges(), label, token) {
            return true;
        }
        // Ordered subsequence, only where the matcher would credit one.
        policy == MatchPolicy::Subsequence && token.chars().count() >= 2
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
        match self.map.as_deref() {
            None => Some((start, end)),
            Some(OffsetMap::Unavailable) => None,
            Some(OffsetMap::Marks { marks, mapped_end }) => {
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

// ---------------------------------------------------------------------------
// Word segmentation and word-prefix decomposition
// ---------------------------------------------------------------------------

/// Most label words the word-prefix DP will consider.
///
/// A label with more words declines the tier outright rather than scoring a
/// truncated prefix of itself. Real application labels are two to four words.
pub const MAX_WORD_PREFIX_WORDS: usize = 8;

/// Longest token the word-prefix DP will consider, in characters.
///
/// Over-cap tokens decline the tier. Truncating the *token* instead would be
/// unsound: a partition of the first `n` characters is not a partition of the
/// token, so `vscodezz` would match `Visual Studio Code` as though the trailing
/// characters had never been typed.
pub const MAX_WORD_PREFIX_TOKEN: usize = 12;

/// Value deducted for each label word the decomposition steps over.
const WORD_SKIP_PENALTY: f32 = 0.6;

/// Whether a character belongs to a word.
///
/// Combining marks count: they attach to the preceding letter, so treating them
/// as separators would split `e` + `U+0301` into two words and let a query match
/// across the seam.
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || is_mark(ch)
}

/// Whether `previous` -> `current` opens a new word.
///
/// `next` is the character after `current`, used only to split an acronym run
/// from a following capitalized word (`VSCode` -> `vs` + `code`).
fn opens_word(previous: Option<char>, current: char, next: Option<char>) -> bool {
    // A mark continues whatever it attaches to and can never start a word.
    if !current.is_alphanumeric() {
        return false;
    }
    let Some(previous) = previous else {
        return true;
    };
    if !is_word_char(previous) {
        return true;
    }
    if previous.is_lowercase() && current.is_uppercase() {
        return true;
    }
    if previous.is_alphabetic() && current.is_numeric() {
        return true;
    }
    if previous.is_numeric() && current.is_alphabetic() {
        return true;
    }
    previous.is_uppercase() && current.is_uppercase() && next.is_some_and(|next| next.is_lowercase())
}

/// Byte ranges of `raw`'s words, expressed as offsets into `label`.
///
/// Segmentation runs on `raw` because folding erases the case transitions that
/// separate `PowerShell` into `power` and `shell`. Offsets are then translated
/// into normalized space, which is what the matcher searches.
fn word_ranges(raw: &str, label: &str, map: Option<&OffsetMap>) -> Box<[(u32, u32)]> {
    // One spare slot so an over-cap label is detectable from the stored list.
    let capacity = MAX_WORD_PREFIX_WORDS + 1;
    let mut ranges: Vec<(u32, u32)> = Vec::new();

    match map {
        // ASCII folds byte for byte, so raw offsets are already label offsets.
        None => {
            let bytes = raw.as_bytes();
            let mut index = 0usize;
            while index < bytes.len() && ranges.len() < capacity {
                let current = bytes[index] as char;
                let previous = (index > 0).then(|| bytes[index - 1] as char);
                let next = bytes.get(index + 1).map(|byte| *byte as char);
                if opens_word(previous, current, next) {
                    let start = index;
                    let mut end = index + 1;
                    while end < bytes.len() {
                        let candidate = bytes[end] as char;
                        let following = bytes.get(end + 1).map(|byte| *byte as char);
                        if !candidate.is_alphanumeric()
                            || opens_word(Some(bytes[end - 1] as char), candidate, following)
                        {
                            break;
                        }
                        end += 1;
                    }
                    if end <= label.len() {
                        ranges.push((start as u32, end as u32));
                    }
                    index = end;
                    continue;
                }
                index += 1;
            }
        }
        // Folding changed byte lengths: walk the marks, which carry each
        // normalized character's raw source range.
        Some(OffsetMap::Marks { marks, mapped_end }) => {
            let mut open: Option<u32> = None;
            let mut previous: Option<char> = None;
            for (position, mark) in marks.iter().enumerate() {
                if mark.norm >= *mapped_end {
                    break;
                }
                let current = raw[mark.src_start as usize..mark.src_end as usize].chars().next();
                let Some(current) = current else { continue };
                let next = marks
                    .get(position + 1)
                    .and_then(|mark| raw[mark.src_start as usize..mark.src_end as usize].chars().next());
                let norm_end = marks
                    .get(position + 1)
                    .map_or(*mapped_end, |following| following.norm);
                if opens_word(previous, current, next) {
                    if let Some(start) = open.take() {
                        ranges.push((start, mark.norm));
                    }
                    if ranges.len() >= capacity {
                        open = None;
                        break;
                    }
                    open = Some(mark.norm);
                } else if !is_word_char(current) {
                    if let Some(start) = open.take() {
                        ranges.push((start, mark.norm));
                    }
                }
                previous = Some(current);
                if open.is_some() && position + 1 == marks.len() {
                    let start = open.take().unwrap_or(mark.norm);
                    ranges.push((start, norm_end));
                }
            }
            if let Some(start) = open {
                if ranges.len() < capacity {
                    ranges.push((start, *mapped_end));
                }
            }
        }
        // No usable mapping: fall back to segmenting the folded text. Case
        // transitions are gone, so camel-case labels contribute one word.
        Some(OffsetMap::Unavailable) => {
            let mut open: Option<usize> = None;
            for (offset, current) in label.char_indices() {
                if is_word_char(current) {
                    if open.is_none() {
                        open = Some(offset);
                    }
                } else if let Some(start) = open.take() {
                    ranges.push((start as u32, offset as u32));
                    if ranges.len() >= capacity {
                        return ranges.into_boxed_slice();
                    }
                }
            }
            if let Some(start) = open {
                ranges.push((start as u32, label.len() as u32));
            }
        }
    }

    ranges.truncate(capacity);
    ranges.into_boxed_slice()
}

/// Longest token the decomposition grid is sized for, in bytes.
const MAX_WORD_PREFIX_TOKEN_BYTES: usize = MAX_WORD_PREFIX_TOKEN * 4;

/// Cells in the decomposition grid: one row per word plus a base row, one
/// column per token byte offset plus the empty suffix.
const WORD_PREFIX_GRID: usize = (MAX_WORD_PREFIX_WORDS + 1) * (MAX_WORD_PREFIX_TOKEN_BYTES + 1);

/// Whether `token`'s characters occur in `label` in order, not necessarily
/// adjacently.
///
/// Necessary for every label-side reading the matcher supports: prefixes and
/// substrings are contiguous runs, and a word-prefix decomposition consumes its
/// chunks left to right across words in increasing order.
fn is_ordered_subsequence(label: &str, token: &str) -> bool {
    let mut haystack = label.chars();
    token
        .chars()
        .all(|needle| haystack.by_ref().any(|character| character == needle))
}

/// Necessary condition for a word-prefix decomposition, without running one.
///
/// For a token of two or more characters, any partition either opens with a
/// chunk of two or more characters — so some word carries the first two
/// characters as a prefix — or opens with a single character, in which case the
/// second character begins a *later* word. Both halves are required: the first
/// covers `so` over `Sound Recorder`, the second covers `vscode` over
/// `Visual Studio Code`, and each alone rejects real matches the other admits.
///
/// Cheap and allocation-free by design. Candidate filtering runs this for every
/// retained position on every keystroke, where the grid would not fit the
/// budget; the grid then confirms or rejects the survivors during scoring.
fn may_decompose(words: &[(u32, u32)], label: &str, token: &str) -> bool {
    let mut characters = token.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    let Some(second) = characters.next() else {
        // A single character can only form a one-chunk decomposition, which the
        // matcher reports as a prefix or substring rather than a word prefix.
        return false;
    };

    let text = |&(start, end): &(u32, u32)| label.get(start as usize..end as usize).unwrap_or_default();

    // A: some word carries both characters as its prefix.
    if words.iter().any(|range| {
        let mut characters = text(range).chars();
        characters.next() == Some(first) && characters.next() == Some(second)
    }) {
        return true;
    }

    // B: a `first`-initial word strictly precedes a `second`-initial word.
    let opens_with = |range: &(u32, u32), wanted: char| text(range).starts_with(wanted);
    match words.iter().position(|range| opens_with(range, first)) {
        Some(index) => words[index + 1..].iter().any(|range| opens_with(range, second)),
        None => false,
    }
}

/// Best decomposition value of `token` over `words`, or `None` when no
/// partition exists.
///
/// A partition splits `token` into consecutive non-empty chunks, each a prefix
/// of a distinct word, words consumed left to right. Value rewards long chunks
/// and whole-word chunks and charges [`WORD_SKIP_PENALTY`] per skipped word, so
/// `vscode` prefers `v|s|code` over any sparser reading.
///
/// The grid is a fixed stack buffer rather than a heap allocation: at
/// `MAX_WORD_PREFIX_WORDS` words and `MAX_WORD_PREFIX_TOKEN` characters it is
/// under two kilobytes, and allocating one per candidate measured eight times
/// the cost of the arithmetic it serves. Only the base row is initialized —
/// every other cell is written before it is read, because the fill runs
/// backwards.
fn word_prefix_into(
    words: &[(u32, u32)],
    label: &str,
    token: &str,
    grid: &mut [f32; WORD_PREFIX_GRID],
) -> Option<(f32, usize)> {
    let word_count = words.len();
    let token_len = token.len();
    if token_len == 0 || word_count == 0 || word_count > MAX_WORD_PREFIX_WORDS {
        return None;
    }
    if token.chars().count() > MAX_WORD_PREFIX_TOKEN {
        return None;
    }
    debug_assert!(token_len <= MAX_WORD_PREFIX_TOKEN_BYTES);

    // `grid[w * stride + q]` is the best value covering `token[q..]` using words
    // `w..`. Filled backwards so each cell reads only already-written cells.
    let stride = token_len + 1;
    let base = word_count * stride;
    grid[base..base + stride].fill(f32::NEG_INFINITY);
    grid[base + token_len] = 0.0;

    for word in (0..word_count).rev() {
        let (start, end) = words[word];
        let text = label.get(start as usize..end as usize).unwrap_or_default();
        let row = word * stride;
        let next = row + stride;
        grid[row + token_len] = 0.0;
        for offset in (0..token_len).rev() {
            // Offsets inside a multi-byte character are not reachable chunk
            // boundaries. They must still be written, because the skip
            // transition below reads the same column of the next row.
            if !token.is_char_boundary(offset) {
                grid[row + offset] = f32::NEG_INFINITY;
                continue;
            }
            let skipped = grid[next + offset];
            let mut best = if skipped == f32::NEG_INFINITY {
                f32::NEG_INFINITY
            } else {
                skipped - WORD_SKIP_PENALTY
            };
            // Viable chunk lengths are exactly the prefixes of the longest
            // common prefix, so it is computed once per cell. It is truncated to
            // a character boundary of the token, so `offset + length` is one too.
            let reach = common_prefix_len(&token[offset..], text);
            for length in 1..=reach {
                let rest = grid[next + offset + length];
                if rest == f32::NEG_INFINITY {
                    continue;
                }
                let whole_word = if length == text.len() { 1.0 } else { 0.0 };
                let value = length as f32 + whole_word + rest;
                if value > best {
                    best = value;
                }
            }
            grid[row + offset] = best;
        }
    }

    let value = grid[0];
    (value != f32::NEG_INFINITY).then_some((value, word_count))
}

/// Byte length of the longest common prefix, truncated to a character boundary.
fn common_prefix_len(left: &str, right: &str) -> usize {
    let limit = left.len().min(right.len());
    let (left_bytes, right_bytes) = (left.as_bytes(), right.as_bytes());
    let mut length = 0usize;
    while length < limit && left_bytes[length] == right_bytes[length] {
        length += 1;
    }
    while length > 0 && !left.is_char_boundary(length) {
        length -= 1;
    }
    length
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
        MatchMethod::WordPrefix => (0.60, 0.73),
        MatchMethod::Substring => (0.45, 0.58),
        MatchMethod::Keyword => (0.30, 0.43),
        MatchMethod::Fuzzy => (0.05, 0.17),
    }
}

// Adjacent bands are separated by at least 0.02, which is the invariant
// `crikey-ranking`'s `W_MATCH_POSITION` is calibrated against: the position
// bonus may refine an ordering within a band and tie at a band edge, but it must
// never carry a weaker method past a stronger one. Narrowing any gap below that
// weight silently makes match position able to invert the tier order.
//
// The bound is checked at 0.019 because the band edges are `f32` literals and
// their difference is not exact: `0.90 - 0.88` evaluates just under `0.02`.
const _: () = {
    const fn gap(stronger: MatchMethod, weaker: MatchMethod) -> f32 {
        band(stronger).0 - band(weaker).1
    }
    assert!(gap(MatchMethod::ExactPrefix, MatchMethod::Prefix) >= 0.019);
    assert!(gap(MatchMethod::Prefix, MatchMethod::WordPrefix) >= 0.019);
    assert!(gap(MatchMethod::WordPrefix, MatchMethod::Substring) >= 0.019);
    assert!(gap(MatchMethod::Substring, MatchMethod::Keyword) >= 0.019);
    assert!(gap(MatchMethod::Keyword, MatchMethod::Fuzzy) >= 0.019);
};

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
/// Prefix, word-prefix and substring matching run against the label, which is
/// the only field highlights can point at. Keyword matching additionally covers
/// the fields a plugin submits for search — `search_terms` and `description` —
/// by containment only. The `target` is deliberately excluded: it is an
/// execution payload (a path, URL or command line), and matching it would make
/// every `/usr/bin/...` item answer to `usr`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultMatcher {
    policy: MatchPolicy,
}

impl DefaultMatcher {
    /// A matcher that credits every reading except ordered subsequence.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            policy: MatchPolicy::Strict,
        }
    }

    /// A matcher that additionally credits ordered-subsequence matches as
    /// [`MatchMethod::Fuzzy`] (spec 11.1).
    ///
    /// Opt-in because subsequence matching cannot be made selective: it admits
    /// `manic` for `Memory Diagnostic Tool` on the same evidence that admits
    /// `vscode` for `Visual Studio Code`, and the two are one part in five
    /// hundred apart. Callers that enable it must also narrow candidate sets
    /// with [`PreparedLabel::may_match_with`] under the same policy, or a later
    /// keystroke will silently drop the subsequence-only candidates.
    #[must_use]
    pub const fn with_subsequence() -> Self {
        Self {
            policy: MatchPolicy::Subsequence,
        }
    }

    /// The readings this matcher credits.
    #[must_use]
    pub const fn policy(&self) -> MatchPolicy {
        self.policy
    }

    /// Scores a prepared label while reusing caller-owned highlight storage.
    ///
    /// `label` must have been prepared from the same `item`; callers that
    /// cannot guarantee that relationship should use [`Self::match_prepared`],
    /// which validates and rebuilds mismatched buffers before scoring.
    /// `spans` is scratch state, not part of the result. Callers retaining only
    /// the best candidates can materialize full highlights after selection.
    pub fn score_prepared(
        &self,
        query: &NormalizedQuery,
        item: &Item,
        label: &PreparedLabel,
        spans: &mut Vec<(usize, usize)>,
    ) -> Option<MatchSummary> {
        spans.clear();
        if query.tokens.is_empty()
            || !query
                .normalized
                .split_whitespace()
                .eq(query.tokens.iter().map(String::as_str))
        {
            return None;
        }

        let normalized = label.normalized();
        let trimmed = normalized.trim();
        if !trimmed.is_empty() && query.normalized.trim() == trimmed {
            let normalized_start = normalized.len() - normalized.trim_start().len();
            let raw_span = label.to_raw(normalized_start, normalized_start + trimmed.len());
            return Some(MatchSummary {
                score: score_for(MatchMethod::ExactPrefix, 1.0),
                method: MatchMethod::ExactPrefix,
                match_position: raw_span.and_then(|(start, _)| raw_character_position(&item.label, start)),
            });
        }

        let phrase = query.normalized.trim();
        if normalized.starts_with(phrase) {
            let raw_span = label.to_raw(0, phrase.len());
            push_span(spans, raw_span);
            retain_valid_spans(&item.label, spans);
            let quality = ratio(phrase.chars().count(), label.char_len);
            return Some(MatchSummary {
                score: score_for(MatchMethod::Prefix, quality),
                method: MatchMethod::Prefix,
                match_position: raw_span.and_then(|(start, _)| raw_character_position(&item.label, start)),
            });
        }

        let mut view = ItemView {
            label,
            keywords: None,
            item,
        };
        let mut weakest = MatchMethod::ExactPrefix;
        let mut quality_total = 0.0f32;

        for token in &query.tokens {
            let (method, quality) = match_token(&mut view, token, spans, self.policy)?;
            if method.precedence() > weakest.precedence() {
                weakest = method;
            }
            quality_total += unit(quality);
        }

        retain_valid_spans(&item.label, spans);
        let match_position = spans
            .iter()
            .filter_map(|(start, _)| raw_character_position(&item.label, *start))
            .min();
        let quality = quality_total / query.tokens.len() as f32;
        Some(MatchSummary {
            score: score_for(weakest, quality),
            method: weakest,
            match_position,
        })
    }

    /// Matches using a label prepared when the catalog admitted the item.
    ///
    /// If a caller accidentally supplies a prepared buffer for another item,
    /// the matcher rebuilds the candidate's buffer before scoring. This keeps
    /// public misuse from turning into a false match or a wrong highlight.
    ///
    /// Validation is exact rather than probabilistic: the canonical buffer for
    /// `item` is built and compared field for field. Comparing only the folded
    /// text would not do — `words` and `map` are derived from the *raw* label,
    /// and distinct raw labels can fold alike. A buffer prepared from
    /// `PowerShell` would otherwise be accepted for an item labelled
    /// `Powershell`, reporting `psh` as a word-prefix match against a label with
    /// no interior boundary and highlighting bytes that spell nothing.
    ///
    /// The rebuild is the same work the mismatch path already did, so a
    /// mismatched call costs what it always cost; a matching call now pays one
    /// preparation to prove itself. Callers on the hot path should use
    /// [`Self::score_prepared`], which trusts the buffer.
    pub fn match_prepared(
        &self,
        query: &NormalizedQuery,
        item: &Item,
        label: &PreparedLabel,
    ) -> Option<MatchOutcome> {
        let (text, label_bytes) = searchable_text_with_label(item);
        let canonical = PreparedLabel::from_searchable_text(&item.label, text, label_bytes);
        let label = if *label == canonical { label } else { &canonical };
        self.match_prepared_unchecked(query, item, label)
    }

    fn match_prepared_unchecked(
        &self,
        query: &NormalizedQuery,
        item: &Item,
        label: &PreparedLabel,
    ) -> Option<MatchOutcome> {
        let mut spans = Vec::new();
        let summary = self.score_prepared(query, item, label, &mut spans)?;
        if summary.method == MatchMethod::ExactPrefix {
            let mut outcome = exact_label_match(query, label)?;
            retain_valid_spans(&item.label, &mut outcome.highlights);
            return Some(outcome);
        }
        retain_valid_spans(&item.label, &mut spans);
        Some(MatchOutcome {
            score: summary.score,
            method: summary.method,
            highlights: merge_spans(spans),
        })
    }
}

impl Matcher for DefaultMatcher {
    fn match_item(&self, query: &NormalizedQuery, item: &Item) -> Option<MatchOutcome> {
        let (text, label_bytes) = searchable_text_with_label(item);
        let label = PreparedLabel::from_searchable_text(&item.label, text, label_bytes);
        self.match_prepared_unchecked(query, item, &label)
    }
}

/// Per-item state shared by every token of one query.
///
/// The label is folded once; the keyword fields are folded lazily, so a query
/// whose tokens all land on the label never pays for them.
#[derive(Debug)]
struct ItemView<'a> {
    label: &'a PreparedLabel,
    keywords: Option<Vec<Cow<'a, str>>>,
    item: &'a Item,
}

impl<'a> ItemView<'a> {
    fn keywords(&mut self) -> &[Cow<'a, str>] {
        let item = self.item;
        self.keywords
            .get_or_insert_with(|| {
                let mut fields = Vec::with_capacity(item.search_terms.len().saturating_add(1));
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
fn exact_label_match(query: &NormalizedQuery, label: &PreparedLabel) -> Option<MatchOutcome> {
    let trimmed = label.normalized().trim();
    if trimmed.is_empty() || query.normalized.trim() != trimmed {
        return None;
    }
    let start = label.normalized().len() - label.normalized().trim_start().len();
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
    policy: MatchPolicy,
) -> Option<(MatchMethod, f32)> {
    // The normalizer never emits one, but `NormalizedQuery` is constructible by
    // hand: an empty token carries no evidence, so it cannot license a match.
    if token.is_empty() {
        return None;
    }

    let token_chars = token.chars().count();

    if let Some(found) = match_label(view.label, token, token_chars, spans) {
        return Some(found);
    }

    // Keyword containment over plugin-supplied fields. It earns no label
    // highlights: the hit is in a description or search term, not in the label
    // the UI underlines.
    if view.label.keywords().contains(token) {
        if let Some(quality) = keyword_quality(view.keywords(), token, token_chars) {
            return Some((MatchMethod::Keyword, quality));
        }
    }

    // Ordered subsequence: the weakest reading, and opt-in because it cannot be
    // made selective.
    if policy == MatchPolicy::Subsequence {
        return subsequence_quality(view.label, token, token_chars, spans)
            .map(|quality| (MatchMethod::Fuzzy, quality));
    }

    None
}

/// Prefix, word-prefix and substring matching, in precedence order.
fn match_label(
    label: &PreparedLabel,
    token: &str,
    token_chars: usize,
    spans: &mut Vec<(usize, usize)>,
) -> Option<(MatchMethod, f32)> {
    if label.normalized().starts_with(token) {
        push_span(spans, label.to_raw(0, token.len()));
        return Some((MatchMethod::Prefix, ratio(token_chars, label.char_len)));
    }

    // The grid is only worth building for a candidate that could decompose.
    // Scoring runs over the whole indexed admission set, not just the warm
    // cache, so this gate belongs here as well as in `may_match_with`.
    if may_decompose(label.word_ranges(), label.normalized(), token) {
        if let Some(quality) = word_prefix_quality(label, token, spans) {
            return Some((MatchMethod::WordPrefix, quality));
        }
    }

    if let Some(at) = label.normalized().find(token) {
        push_span(spans, label.to_raw(at, at.saturating_add(token.len())));
        let coverage = ratio(token_chars, label.char_len);
        // A hit close to the front of the label reads as more relevant.
        let position = 1.0 / label.normalized()[..at].chars().count().saturating_add(1) as f32;
        return Some((MatchMethod::Substring, 0.5 * coverage + 0.5 * position));
    }

    None
}

/// The token splits into two or more chunks that are each a prefix of a
/// distinct label word.
///
/// Highlights cover the matched chunks, recovered by replaying the choices the
/// value grid made. Quality normalizes the decomposition value by the best a
/// token of this length over a label of this many words could earn, so a tight
/// reading of a short label outscores a sparse reading of a long one.
///
/// A single-chunk decomposition is deliberately rejected: it is a substring
/// anchored at a word start, not an abbreviation spanning words, and the
/// substring tier already scores it by position.
fn word_prefix_quality(label: &PreparedLabel, token: &str, spans: &mut Vec<(usize, usize)>) -> Option<f32> {
    let words = label.word_ranges();
    let text = label.normalized();
    // Stack-resident: see `word_prefix_into`. Only the base row is initialized
    // there, so the zeroes here are never read as values.
    let mut grid = [0.0f32; WORD_PREFIX_GRID];
    let (value, word_count) = word_prefix_into(words, text, token, &mut grid)?;

    let stride = token.len() + 1;
    let mark = spans.len();
    let mut offset = 0usize;
    let mut chunks = 0usize;

    for word in 0..word_count {
        if offset == token.len() {
            break;
        }
        let (start, end) = words[word];
        let word_text = text.get(start as usize..end as usize).unwrap_or_default();
        let expected = grid[word * stride + offset];
        let reach = common_prefix_len(&token[offset..], word_text);
        // Recover which chunk length the grid credited at this cell, if any.
        let taken = (1..=reach).find(|&length| {
            let rest = grid[(word + 1) * stride + offset + length];
            if rest == f32::NEG_INFINITY {
                return false;
            }
            let whole_word = if length == word_text.len() { 1.0 } else { 0.0 };
            (length as f32 + whole_word + rest - expected).abs() < 1e-4
        });
        let Some(taken) = taken else { continue };
        push_span(spans, label.to_raw(start as usize, start as usize + taken));
        chunks += 1;
        offset += taken;
    }

    if offset != token.len() || chunks < 2 {
        spans.truncate(mark);
        return None;
    }

    let ideal = token.chars().count() as f32 + word_count as f32;
    Some(unit(value.max(0.0) / ideal))
}

/// The token's characters occur in the label in order, not necessarily adjacent
/// and not necessarily at word boundaries (spec 11.1 fuzzy matching).
///
/// Only reached under [`MatchPolicy::Subsequence`]. The quality term below is
/// the best available separation of an intentional abbreviation from a
/// coincidence, and it is not good enough to rank on alone: measured over a
/// realistic catalog, `vscode` against `Visual Studio Code` scores 0.667 and
/// `manic` against `Manage Windows Credentials` scores 0.656. That is why the
/// word-prefix tier exists above it and why this one is opt-in — no threshold
/// placed here separates the two classes.
fn subsequence_quality(
    label: &PreparedLabel,
    token: &str,
    token_chars: usize,
    spans: &mut Vec<(usize, usize)>,
) -> Option<f32> {
    // A single character is a substring, not a subsequence.
    if token_chars < 2 {
        return None;
    }

    let mark = spans.len();
    let mut haystack = label.normalized().char_indices().enumerate();
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
    let compactness = ratio(matched, (last_ordinal - first_ordinal).saturating_add(1));
    let earliness = 1.0 / first_ordinal.saturating_add(1) as f32;
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

fn is_mark(ch: char) -> bool {
    !ch.is_ascii()
        && matches!(
            get_general_category(ch),
            GeneralCategory::NonspacingMark | GeneralCategory::SpacingMark | GeneralCategory::EnclosingMark
        )
}

/// Converts a raw-label byte boundary into a saturating character offset.
///
/// Prepared labels normally come from the same item that is being scored, but
/// the public prepared-label API cannot enforce that relationship. Rejecting a
/// boundary that is outside the candidate keeps ranking and matching panic-free
/// when a caller accidentally reuses a label from another item.
fn raw_character_position(label: &str, byte_offset: usize) -> Option<u32> {
    let prefix = label.get(..byte_offset)?;
    Some(u32::try_from(prefix.chars().count()).unwrap_or(u32::MAX))
}

fn retain_valid_spans(label: &str, spans: &mut Vec<(usize, usize)>) {
    spans.retain(|&(start, end)| start < end && label.get(start..end).is_some());
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
