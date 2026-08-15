//! Catalog store (spec 10, 22, 25.1).
//!
//! Target: at least 500,000 indexed items with responsive search.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, Generation, HitPolicy, Item, ItemId,
    PluginId,
};
use crikey_query::{
    presence_mask, searchable_text_with_label, MatchPolicy, NormalizedQuery, PreparedLabel, DEDICATED_BITS,
};

/// Why a catalog lifecycle operation or update was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogError {
    /// The publishing or retiring instance is not the plugin's active instance.
    StaleInstance,
    /// At least one item is not owned by the plugin publishing the update.
    OwnerMismatch,
    /// The incoming vector contains more raw items than one update may process.
    BatchItemLimitExceeded { actual: usize, limit: usize },
    /// The update would retain too many unique items for one plugin.
    PluginItemLimitExceeded { actual: usize, limit: usize },
    /// The update would retain too many unique items across all plugins.
    TotalItemLimitExceeded { actual: usize, limit: usize },
    /// One item, including its nested actions, exceeds the payload limit.
    ItemPayloadLimitExceeded { actual: usize, limit: usize },
    /// The complete incoming batch exceeds the payload limit.
    BatchPayloadLimitExceeded { actual: usize, limit: usize },
    /// Checked payload accounting could not represent the payload size.
    PayloadSizeOverflow,
    /// Checked retained-item accounting could not represent the item count.
    ItemCountOverflow,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleInstance => formatter.write_str("catalog operation came from a stale plugin instance"),
            Self::OwnerMismatch => {
                formatter.write_str("catalog update contains an item owned by another plugin")
            }
            Self::BatchItemLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog update contains {actual} raw items; the limit is {limit}"
                )
            }
            Self::PluginItemLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog update would retain {actual} items for one plugin; the limit is {limit}"
                )
            }
            Self::TotalItemLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog update would retain {actual} total items; the limit is {limit}"
                )
            }
            Self::ItemPayloadLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog item payload is {actual} bytes; the limit is {limit}"
                )
            }
            Self::BatchPayloadLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog batch payload is {actual} bytes; the limit is {limit}"
                )
            }
            Self::PayloadSizeOverflow => formatter.write_str("catalog payload size cannot be represented"),
            Self::ItemCountOverflow => formatter.write_str("catalog item count cannot be represented"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Resource limits applied atomically before a catalog update allocates or
/// mutates retained state.
///
/// Batch item counts include duplicates. Plugin and total counts apply to
/// unique retained stable IDs. Payload bytes use a deterministic,
/// length-prefixed accounting of every [`Item`] and nested [`Action`] field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogLimits {
    pub max_batch_items: usize,
    pub max_plugin_items: usize,
    pub max_total_items: usize,
    pub max_item_bytes: usize,
    pub max_batch_bytes: usize,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            max_batch_items: 500_000,
            max_plugin_items: 500_000,
            max_total_items: 500_000,
            max_item_bytes: 1_048_576,
            max_batch_bytes: 1_073_741_824,
        }
    }
}

/// Ownership of catalog contributions is per plugin so a rebuild or a crashed
/// worker only invalidates its own slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogUpdate {
    /// Legacy `set_catalog()` and modern full rebuild.
    Replace,
    /// Legacy `merge_catalog()`.
    Merge,
}

pub trait CatalogStore {
    /// Makes `instance` the only publisher authorized for `plugin`.
    ///
    /// Instance numbers are a monotonic high-water mark. Repeating the current
    /// number is idempotent (and reactivates a retired instance), while a lower
    /// number is rejected.
    fn activate_instance(&mut self, plugin: &PluginId, instance: u64) -> Result<(), CatalogError>;

    /// Revokes an active instance without discarding its high-water mark or
    /// retained catalog slice.
    fn retire_instance(&mut self, plugin: &PluginId, instance: u64) -> Result<(), CatalogError>;

    /// Removes one plugin's retained items without changing its authorization.
    fn invalidate(&mut self, plugin: &PluginId);

    /// Applies a plugin's catalog contribution. Updates from superseded or
    /// retired plugin instances are rejected (spec 14.8).
    fn apply(
        &mut self,
        plugin: &PluginId,
        instance: u64,
        update: CatalogUpdate,
        items: Vec<Item>,
    ) -> Result<(), CatalogError>;

    /// Returns an item from one plugin's catalog slice.
    fn get(&self, plugin: &PluginId, id: &ItemId) -> Option<&Item>;

    /// Returns the number of retained items owned by `plugin`.
    fn plugin_len(&self, plugin: &PluginId) -> usize;

    /// Returns `plugin`'s retained items in their stable catalog order.
    fn items(&self, plugin: &PluginId) -> &[Item];

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One item's folded searchable text and highlight mapping, prepared when the
/// item entered the catalog, together with its presence mask.
///
/// Folding and Unicode offset alignment are the expensive half of matching.
/// Retaining them here means half a million labels are prepared once at
/// ingestion rather than again on every keystroke.
#[derive(Debug)]
pub struct ItemIndex {
    prepared: PreparedLabel,
    mask: u64,
}

impl ItemIndex {
    fn new(item: &Item) -> Self {
        let (text, label_bytes) = searchable_text_with_label(item);
        let mask = presence_mask(&text);
        Self {
            prepared: PreparedLabel::from_searchable_text(&item.label, text, label_bytes),
            mask,
        }
    }

    /// The item's label, folded.
    pub fn label(&self) -> &str {
        self.prepared.normalized()
    }

    /// The item's description and search terms, folded and joined by single
    /// spaces. Empty when the item carries neither.
    pub fn keywords(&self) -> &str {
        self.prepared.keywords()
    }

    /// Prepared label and Unicode highlight map used by the matcher.
    pub fn prepared_label(&self) -> &PreparedLabel {
        &self.prepared
    }

    /// The characters the label and the keyword text hold between them.
    pub const fn mask(&self) -> u64 {
        self.mask
    }

    /// Whether this item could still match a query whose characters are
    /// `wanted`.
    ///
    /// Every method the matcher supports needs each character of a token to
    /// occur somewhere in the item's searchable text, so an item this rejects
    /// could not have matched: refusing it loses no result (spec 11.1).
    const fn admits(&self, wanted: u64) -> bool {
        (wanted & !self.mask) == 0
    }
}

const ORDERED_PAIR_COUNT: usize = DEDICATED_BITS as usize * DEDICATED_BITS as usize;
const ORDERED_PAIR_WORDS: usize = ORDERED_PAIR_COUNT.div_ceil(u64::BITS as usize);

fn dedicated_index(character: char) -> Option<usize> {
    match character {
        'a'..='z' => Some((character as u32 - 'a' as u32) as usize),
        '0'..='9' => Some((DEDICATED_BITS - 10 + character as u32 - '0' as u32) as usize),
        _ => None,
    }
}

fn ordered_pair_signature(label: &PreparedLabel) -> [u64; ORDERED_PAIR_WORDS] {
    let mut signature = [0u64; ORDERED_PAIR_WORDS];

    // Label matching admits substrings and word-prefix decompositions, and both
    // consume the token's characters in order, so every earlier dedicated
    // character may constrain every later one.
    let mut seen = 0u64;
    for character in label.normalized().chars() {
        let Some(after) = dedicated_index(character) else {
            continue;
        };
        let mut before_bits = seen;
        while before_bits != 0 {
            let before = before_bits.trailing_zeros() as usize;
            before_bits &= before_bits - 1;
            let pair = before * DEDICATED_BITS as usize + after;
            signature[pair / u64::BITS as usize] |= 1u64 << (pair % u64::BITS as usize);
        }
        seen |= 1u64 << after;
    }

    // Keyword matching is containment only. Adjacent pairs are sufficient,
    // and resetting at separators prevents false pairs across field joins.
    let mut previous = None;
    for character in label.keywords().chars() {
        let current = dedicated_index(character);
        if let (Some(before), Some(after)) = (previous, current) {
            let pair = before * DEDICATED_BITS as usize + after;
            signature[pair / u64::BITS as usize] |= 1u64 << (pair % u64::BITS as usize);
        }
        previous = current;
    }
    signature
}

fn query_ordered_pairs(query: &NormalizedQuery) -> Vec<usize> {
    let mut pairs = Vec::new();
    for token in &query.tokens {
        let mut previous = None;
        for character in token.chars() {
            let current = dedicated_index(character);
            if let (Some(before), Some(after)) = (previous, current) {
                let pair = before * DEDICATED_BITS as usize + after;
                if !pairs.contains(&pair) {
                    pairs.push(pair);
                }
            }
            previous = current;
        }
    }
    pairs
}

fn prefix_pair(text: &str) -> Option<usize> {
    let mut characters = text.chars();
    let before = dedicated_index(characters.next()?)?;
    let after = dedicated_index(characters.next()?)?;
    Some(before * DEDICATED_BITS as usize + after)
}

#[derive(Debug)]
struct PluginCatalog {
    items: Vec<Item>,
    /// The [`ItemIndex`] of `items`, position for position.
    index: Vec<ItemIndex>,
    positions: HashMap<ItemId, usize>,
    /// One dense bitset per dedicated ASCII letter/digit presence bit.
    postings: [Vec<u64>; DEDICATED_BITS as usize],
    /// Items whose searchable text contains each ordered character pair.
    ordered_pair_postings: Vec<Vec<u64>>,
    /// Stable positions grouped by the label's first two folded dedicated
    /// characters (the fast path used for ASCII prefixes).
    prefix_postings: Vec<Vec<usize>>,
}

impl Default for PluginCatalog {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            index: Vec::new(),
            positions: HashMap::new(),
            postings: std::array::from_fn(|_| Vec::new()),
            ordered_pair_postings: (0..ORDERED_PAIR_COUNT).map(|_| Vec::new()).collect(),
            prefix_postings: (0..ORDERED_PAIR_COUNT).map(|_| Vec::new()).collect(),
        }
    }
}

impl PluginCatalog {
    fn from_items(items: Vec<Item>, unique_items: usize) -> Self {
        let posting_words = unique_items.div_ceil(u64::BITS as usize);
        let mut catalog = Self {
            items: Vec::with_capacity(unique_items),
            index: Vec::with_capacity(unique_items),
            positions: HashMap::with_capacity(unique_items),
            postings: std::array::from_fn(|_| Vec::with_capacity(posting_words)),
            ordered_pair_postings: (0..ORDERED_PAIR_COUNT)
                .map(|_| Vec::with_capacity(posting_words))
                .collect(),
            prefix_postings: (0..ORDERED_PAIR_COUNT).map(|_| Vec::new()).collect(),
        };
        let added = catalog.merge(items);
        debug_assert_eq!(added, unique_items);
        catalog
    }

    /// Appends or replaces `items`, folding each exactly once.
    ///
    /// This is the only place a retained item is ever written, and it writes
    /// that item's index entry in the same step, so an entry can neither
    /// outlive the item it describes nor lag a replacement of it.
    fn merge(&mut self, items: Vec<Item>) -> usize {
        let mut added = 0usize;
        let mut dirty_prefixes = [false; ORDERED_PAIR_COUNT];

        for item in items {
            let entry = ItemIndex::new(&item);
            let new_pairs = ordered_pair_signature(entry.prepared_label());
            let new_prefix = prefix_pair(entry.prepared_label().normalized());
            if let Some(&position) = self.positions.get(&item.stable_id) {
                let old_pairs = ordered_pair_signature(self.index[position].prepared_label());
                let old_prefix = prefix_pair(self.index[position].prepared_label().normalized());
                if old_prefix != new_prefix {
                    if let Some(old_prefix) = old_prefix {
                        dirty_prefixes[old_prefix] = true;
                    }
                    if let Some(new_prefix) = new_prefix {
                        dirty_prefixes[new_prefix] = true;
                    }
                }
                self.set_ordered_pair_postings(position, &old_pairs, false);
                self.set_postings(position, entry.mask());
                self.set_ordered_pair_postings(position, &new_pairs, true);
                self.items[position] = item;
                self.index[position] = entry;
            } else {
                let position = self.items.len();
                let displaced = self.positions.insert(item.stable_id.clone(), position);
                debug_assert!(displaced.is_none());
                self.set_postings(position, entry.mask());
                self.set_ordered_pair_postings(position, &new_pairs, true);
                if let Some(new_prefix) = new_prefix {
                    self.prefix_postings[new_prefix].push(position);
                }
                self.items.push(item);
                self.index.push(entry);
                added += 1;
            }
        }

        // Removing one position from a prefix vector with `retain` is linear
        // in that vector. A batch replacing many items with the same prefix
        // would therefore scan the same catalog slice once per item. Rebuild
        // only the affected prefix buckets in one stable-order pass instead.
        if dirty_prefixes.iter().any(|&dirty| dirty) {
            for (posting, &dirty) in self.prefix_postings.iter_mut().zip(dirty_prefixes.iter()) {
                if dirty {
                    posting.clear();
                }
            }
            for (position, entry) in self.index.iter().enumerate() {
                let Some(prefix) = prefix_pair(entry.prepared_label().normalized()) else {
                    continue;
                };
                if dirty_prefixes[prefix] {
                    self.prefix_postings[prefix].push(position);
                }
            }
        }

        debug_assert_eq!(self.items.len(), self.index.len());
        added
    }

    fn set_postings(&mut self, position: usize, mask: u64) {
        let word = position / u64::BITS as usize;
        let position_bit = 1u64 << (position % u64::BITS as usize);
        for (presence_bit, posting) in self.postings.iter_mut().enumerate() {
            if posting.len() <= word {
                posting.resize(word + 1, 0);
            }
            if mask & (1u64 << presence_bit) == 0 {
                posting[word] &= !position_bit;
            } else {
                posting[word] |= position_bit;
            }
        }
    }

    fn set_ordered_pair_postings(
        &mut self,
        position: usize,
        signature: &[u64; ORDERED_PAIR_WORDS],
        present: bool,
    ) {
        let word = position / u64::BITS as usize;
        let position_bit = 1u64 << (position % u64::BITS as usize);
        for (signature_word_index, &signature_word) in signature.iter().enumerate() {
            let mut pairs = signature_word;
            while pairs != 0 {
                let within_word = pairs.trailing_zeros() as usize;
                pairs &= pairs - 1;
                let pair = signature_word_index * u64::BITS as usize + within_word;
                let posting = &mut self.ordered_pair_postings[pair];
                if present {
                    if posting.len() <= word {
                        posting.resize(word + 1, 0);
                    }
                    posting[word] |= position_bit;
                } else if let Some(posting_word) = posting.get_mut(word) {
                    *posting_word &= !position_bit;
                }
            }
        }
    }

    fn get(&self, id: &ItemId) -> Option<&Item> {
        self.positions
            .get(id)
            .and_then(|&position| self.items.get(position))
    }

    fn visit_candidates<'a>(
        &'a self,
        wanted: u64,
        ordered_pairs: &[usize],
        mut visit: impl FnMut(usize, &'a Item, &'a ItemIndex),
    ) {
        const DEDICATED_MASK: u64 = (1u64 << DEDICATED_BITS) - 1;
        let dedicated = wanted & DEDICATED_MASK;

        if dedicated == 0 && ordered_pairs.is_empty() {
            for (position, (item, entry)) in self.items.iter().zip(&self.index).enumerate() {
                if entry.admits(wanted) {
                    visit(position, item, entry);
                }
            }
            return;
        }

        let word_count = self.items.len().div_ceil(u64::BITS as usize);
        for word_index in 0..word_count {
            let mut positions = u64::MAX;
            let mut required = dedicated;
            while required != 0 {
                let bit = required.trailing_zeros() as usize;
                positions &= self.postings[bit][word_index];
                required &= required - 1;
            }
            for &pair in ordered_pairs {
                positions &= self.ordered_pair_postings[pair]
                    .get(word_index)
                    .copied()
                    .unwrap_or(0);
            }

            while positions != 0 {
                let within_word = positions.trailing_zeros() as usize;
                positions &= positions - 1;
                let position = word_index * u64::BITS as usize + within_word;
                let Some((item, entry)) = self.items.get(position).zip(self.index.get(position)) else {
                    continue;
                };
                if entry.admits(wanted) {
                    visit(position, item, entry);
                }
            }
        }
    }

    /// The retained items whose searchable text holds every character in
    /// `wanted`, in stable catalog order.
    fn candidates(&self, wanted: u64) -> Vec<&Item> {
        let mut candidates = Vec::new();
        self.visit_candidates(wanted, &[], |_, item, _| candidates.push(item));
        candidates
    }
    fn visit_prefix<'a>(&'a self, token: &str, mut visit: impl FnMut(usize, &'a Item, &'a PreparedLabel)) {
        if token.is_empty() {
            return;
        }

        if let Some(prefix) = prefix_pair(token) {
            for &position in &self.prefix_postings[prefix] {
                let Some((item, entry)) = self.items.get(position).zip(self.index.get(position)) else {
                    continue;
                };
                if entry.prepared_label().normalized().starts_with(token) {
                    visit(position, item, entry.prepared_label());
                }
            }
            return;
        }

        // The compact prefix postings only cover two dedicated ASCII
        // characters. Unicode prefixes, one-character prefixes, and prefixes
        // beginning with a shared-bucket character use a complete scan so the
        // optimization never changes the result set.
        for (position, (item, entry)) in self.items.iter().zip(&self.index).enumerate() {
            if entry.prepared_label().normalized().starts_with(token) {
                visit(position, item, entry.prepared_label());
            }
        }
    }
}

const LENGTH_PREFIX_BYTES: usize = 8;
const ENUM_TAG_BYTES: usize = 1;
const SCORE_HINT_BYTES: usize = 4;

fn add_payload_bytes(total: &mut usize, bytes: usize) -> Result<(), CatalogError> {
    *total = total
        .checked_add(bytes)
        .ok_or(CatalogError::PayloadSizeOverflow)?;
    Ok(())
}

fn add_string_payload(total: &mut usize, value: &str) -> Result<(), CatalogError> {
    add_payload_bytes(total, LENGTH_PREFIX_BYTES)?;
    add_payload_bytes(total, value.len())
}

fn add_category_payload(total: &mut usize, category: &Category) -> Result<(), CatalogError> {
    add_payload_bytes(total, ENUM_TAG_BYTES)?;
    if let Category::PluginDefined(name) = category {
        add_string_payload(total, name)?;
    }
    Ok(())
}

fn add_optional_string_payload(total: &mut usize, value: Option<&str>) -> Result<(), CatalogError> {
    add_payload_bytes(total, ENUM_TAG_BYTES)?;
    if let Some(value) = value {
        add_string_payload(total, value)?;
    }
    Ok(())
}

fn add_action_payload(total: &mut usize, action: &Action) -> Result<(), CatalogError> {
    add_string_payload(total, &action.action_id.0)?;
    add_string_payload(total, &action.label)?;
    add_string_payload(total, &action.description)?;
    add_payload_bytes(total, LENGTH_PREFIX_BYTES)?;
    for category in &action.applicable_categories {
        add_category_payload(total, category)?;
    }
    add_optional_string_payload(total, action.icon_reference.as_deref())?;
    add_payload_bytes(total, ENUM_TAG_BYTES)
}

fn item_payload_bytes(item: &Item) -> Result<usize, CatalogError> {
    let mut total = 0usize;
    add_string_payload(&mut total, &item.stable_id.0)?;
    add_string_payload(&mut total, &item.plugin_id.0)?;
    add_category_payload(&mut total, &item.category)?;
    add_string_payload(&mut total, &item.label)?;
    add_string_payload(&mut total, &item.description)?;
    add_string_payload(&mut total, &item.target)?;

    add_payload_bytes(&mut total, LENGTH_PREFIX_BYTES)?;
    for term in &item.search_terms {
        add_string_payload(&mut total, term)?;
    }
    add_optional_string_payload(&mut total, item.icon_reference.as_deref())?;
    add_payload_bytes(&mut total, ENUM_TAG_BYTES)?;
    add_payload_bytes(&mut total, ENUM_TAG_BYTES)?;
    add_payload_bytes(&mut total, SCORE_HINT_BYTES)?;

    add_payload_bytes(&mut total, LENGTH_PREFIX_BYTES)?;
    for (key, value) in &item.metadata {
        add_string_payload(&mut total, key)?;
        add_string_payload(&mut total, value)?;
    }

    add_payload_bytes(&mut total, LENGTH_PREFIX_BYTES)?;
    for action in &item.actions {
        add_action_payload(&mut total, action)?;
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy)]
struct PluginInstance {
    high_water: u64,
    active: bool,
}

#[derive(Debug, Clone, Copy)]
struct UpdatePlan {
    unique_batch_items: usize,
    merge_additions: usize,
    projected_total_items: usize,
}

/// Owner-scoped in-memory catalog with stable per-plugin item ordering.
#[derive(Debug, Default)]
pub struct MemoryCatalog {
    instances: HashMap<PluginId, PluginInstance>,
    plugins: HashMap<PluginId, PluginCatalog>,
    item_count: usize,
    limits: CatalogLimits,
}

impl MemoryCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: CatalogLimits) -> Self {
        Self {
            instances: HashMap::new(),
            plugins: HashMap::new(),
            item_count: 0,
            limits,
        }
    }

    /// The items of `plugin` that could still match `query`, in stable catalog
    /// order and each offered once.
    ///
    /// This is the candidate prefilter of spec 11.1. Every method the matcher
    /// supports needs each character of a query token to occur somewhere in
    /// the item's searchable text, so an item whose presence mask is missing
    /// one of them cannot match however it is scored. What comes back is
    /// therefore a superset of the true match set: pruning changes what a
    /// query costs, never what it answers.
    ///
    /// A query with no tokens constrains nothing and hands back the whole
    /// slice. A plugin with no retained slice — one that never published, or
    /// whose slice was invalidated — offers nothing.
    pub fn candidates(&self, plugin: &PluginId, query: &NormalizedQuery) -> Vec<&Item> {
        let Some(catalog) = self.plugins.get(plugin) else {
            return Vec::new();
        };

        // Every token has to be admitted on its own, and `& !mask` distributes
        // over `|`, so the union of the token masks settles each item in a
        // single test.
        let wanted = query
            .tokens
            .iter()
            .fold(0u64, |wanted, token| wanted | presence_mask(token));

        catalog.candidates(wanted)
    }

    /// Visits candidate items with labels prepared at catalog-ingestion time.
    ///
    /// The visitor avoids allocating a potentially large intermediate vector
    /// on every keystroke. It receives a sound indexed superset and must run
    /// the matcher once to decide and score each candidate.
    pub fn visit_prepared_candidates<'a>(
        &'a self,
        plugin: &PluginId,
        query: &NormalizedQuery,
        visit: impl FnMut(usize, &'a Item, &'a PreparedLabel),
    ) {
        let Some(catalog) = self.plugins.get(plugin) else {
            return;
        };
        let wanted = query
            .tokens
            .iter()
            .fold(0u64, |wanted, token| wanted | presence_mask(token));
        let ordered_pairs = query_ordered_pairs(query);
        let mut visit = visit;
        catalog.visit_candidates(wanted, &ordered_pairs, |position, item, entry| {
            visit(position, item, entry.prepared_label());
        });
    }

    /// Visits labels beginning with the complete normalized query token.
    pub fn visit_label_prefixes<'a>(
        &'a self,
        plugin: &PluginId,
        token: &str,
        visit: impl FnMut(usize, &'a Item, &'a PreparedLabel),
    ) {
        if let Some(catalog) = self.plugins.get(plugin) {
            catalog.visit_prefix(token, visit);
        }
    }

    /// Revisits positions accepted by an earlier query from the same catalog,
    /// under the default [`MatchPolicy::Strict`].
    ///
    /// A caller may use this only when the new normalized query extends the
    /// previous one, making its match set a subset of the previous match set.
    pub fn visit_prepared_positions<'a>(
        &'a self,
        plugin: &PluginId,
        positions: &[usize],
        query: &NormalizedQuery,
        visit: impl FnMut(usize, &'a Item, &'a PreparedLabel),
    ) {
        self.visit_prepared_positions_with(plugin, positions, query, MatchPolicy::Strict, visit);
    }

    /// [`Self::visit_prepared_positions`] under an explicit policy.
    ///
    /// The policy must be the one the matcher runs under, and the same one the
    /// retained `positions` were admitted under. Narrowing a set with a stricter
    /// policy than the matcher uses drops candidates the matcher would have
    /// accepted: a [`MatchPolicy::Subsequence`] pass whose warm keystrokes filter
    /// strictly would match a subsequence-only item on the first character and
    /// then lose it on the second, which reads as results flickering out as the
    /// user types.
    pub fn visit_prepared_positions_with<'a>(
        &'a self,
        plugin: &PluginId,
        positions: &[usize],
        query: &NormalizedQuery,
        policy: MatchPolicy,
        mut visit: impl FnMut(usize, &'a Item, &'a PreparedLabel),
    ) {
        let Some(catalog) = self.plugins.get(plugin) else {
            return;
        };
        for &position in positions {
            let Some((item, entry)) = catalog.items.get(position).zip(catalog.index.get(position)) else {
                continue;
            };
            if entry.prepared_label().may_match_with(query, policy) {
                visit(position, item, entry.prepared_label());
            }
        }
    }

    /// The folded searchable text and presence mask of every item `plugin`
    /// retains, position for position with [`items`](CatalogStore::items).
    pub fn item_index(&self, plugin: &PluginId) -> &[ItemIndex] {
        match self.plugins.get(plugin) {
            Some(catalog) => catalog.index.as_slice(),
            None => &[],
        }
    }

    fn plan_update(
        &self,
        plugin: &PluginId,
        update: CatalogUpdate,
        items: &[Item],
    ) -> Result<UpdatePlan, CatalogError> {
        if items.len() > self.limits.max_batch_items {
            return Err(CatalogError::BatchItemLimitExceeded {
                actual: items.len(),
                limit: self.limits.max_batch_items,
            });
        }

        if items.iter().any(|item| &item.plugin_id != plugin) {
            return Err(CatalogError::OwnerMismatch);
        }

        let mut batch_bytes = 0usize;
        for item in items {
            let item_bytes = item_payload_bytes(item)?;
            if item_bytes > self.limits.max_item_bytes {
                return Err(CatalogError::ItemPayloadLimitExceeded {
                    actual: item_bytes,
                    limit: self.limits.max_item_bytes,
                });
            }
            batch_bytes = batch_bytes
                .checked_add(item_bytes)
                .ok_or(CatalogError::PayloadSizeOverflow)?;
        }
        if batch_bytes > self.limits.max_batch_bytes {
            return Err(CatalogError::BatchPayloadLimitExceeded {
                actual: batch_bytes,
                limit: self.limits.max_batch_bytes,
            });
        }

        let mut unique_ids = HashSet::new();
        for item in items {
            unique_ids.insert(&item.stable_id);
        }

        let existing = self.plugins.get(plugin);
        let previous_plugin_items = existing.map_or(0, |catalog| catalog.items.len());
        let (projected_plugin_items, merge_additions) = match update {
            CatalogUpdate::Replace => (unique_ids.len(), 0),
            CatalogUpdate::Merge => {
                let mut additions = 0usize;
                for id in &unique_ids {
                    if existing.is_none_or(|catalog| !catalog.positions.contains_key(*id)) {
                        additions = additions.checked_add(1).ok_or(CatalogError::ItemCountOverflow)?;
                    }
                }
                let projected = previous_plugin_items
                    .checked_add(additions)
                    .ok_or(CatalogError::ItemCountOverflow)?;
                (projected, additions)
            }
        };

        if projected_plugin_items > self.limits.max_plugin_items {
            return Err(CatalogError::PluginItemLimitExceeded {
                actual: projected_plugin_items,
                limit: self.limits.max_plugin_items,
            });
        }

        let other_plugin_items = self
            .item_count
            .checked_sub(previous_plugin_items)
            .ok_or(CatalogError::ItemCountOverflow)?;
        let projected_total_items = other_plugin_items
            .checked_add(projected_plugin_items)
            .ok_or(CatalogError::ItemCountOverflow)?;
        if projected_total_items > self.limits.max_total_items {
            return Err(CatalogError::TotalItemLimitExceeded {
                actual: projected_total_items,
                limit: self.limits.max_total_items,
            });
        }

        Ok(UpdatePlan {
            unique_batch_items: unique_ids.len(),
            merge_additions,
            projected_total_items,
        })
    }
}

impl CatalogStore for MemoryCatalog {
    fn activate_instance(&mut self, plugin: &PluginId, instance: u64) -> Result<(), CatalogError> {
        if let Some(state) = self.instances.get_mut(plugin) {
            if instance < state.high_water {
                return Err(CatalogError::StaleInstance);
            }
            state.high_water = instance;
            state.active = true;
        } else {
            let displaced = self.instances.insert(
                plugin.clone(),
                PluginInstance {
                    high_water: instance,
                    active: true,
                },
            );
            debug_assert!(displaced.is_none());
        }
        Ok(())
    }

    fn retire_instance(&mut self, plugin: &PluginId, instance: u64) -> Result<(), CatalogError> {
        match self.instances.get_mut(plugin) {
            Some(state) if state.active && state.high_water == instance => {
                state.active = false;
                Ok(())
            }
            _ => Err(CatalogError::StaleInstance),
        }
    }

    fn invalidate(&mut self, plugin: &PluginId) {
        if let Some(catalog) = self.plugins.remove(plugin) {
            self.item_count -= catalog.items.len();
        }
    }

    fn apply(
        &mut self,
        plugin: &PluginId,
        instance: u64,
        update: CatalogUpdate,
        items: Vec<Item>,
    ) -> Result<(), CatalogError> {
        let is_active = self
            .instances
            .get(plugin)
            .is_some_and(|state| state.active && state.high_water == instance);
        if !is_active {
            return Err(CatalogError::StaleInstance);
        }

        let plan = self.plan_update(plugin, update, &items)?;

        match update {
            CatalogUpdate::Replace => {
                if plan.unique_batch_items == 0 {
                    self.plugins.remove(plugin);
                } else {
                    let replacement = PluginCatalog::from_items(items, plan.unique_batch_items);
                    self.plugins.insert(plugin.clone(), replacement);
                }
            }
            CatalogUpdate::Merge => {
                if plan.unique_batch_items == 0 {
                    return Ok(());
                }

                if let Some(catalog) = self.plugins.get_mut(plugin) {
                    catalog.items.reserve(plan.merge_additions);
                    catalog.index.reserve(plan.merge_additions);
                    catalog.positions.reserve(plan.merge_additions);
                    let added = catalog.merge(items);
                    debug_assert_eq!(added, plan.merge_additions);
                } else {
                    let catalog = PluginCatalog::from_items(items, plan.unique_batch_items);
                    let displaced = self.plugins.insert(plugin.clone(), catalog);
                    debug_assert!(displaced.is_none());
                }
            }
        }

        self.item_count = plan.projected_total_items;
        Ok(())
    }

    fn get(&self, plugin: &PluginId, id: &ItemId) -> Option<&Item> {
        self.plugins.get(plugin).and_then(|catalog| catalog.get(id))
    }

    fn plugin_len(&self, plugin: &PluginId) -> usize {
        self.plugins.get(plugin).map_or(0, |catalog| catalog.items.len())
    }

    fn items(&self, plugin: &PluginId) -> &[Item] {
        match self.plugins.get(plugin) {
            Some(catalog) => catalog.items.as_slice(),
            None => &[],
        }
    }

    fn len(&self) -> usize {
        self.item_count
    }
}

// ---------------------------------------------------------------------------
// Persistent per-plugin catalog cache (spec 22.1, 22.4, 25.6)
//
// ADR-0008 governs the shape defended here: one slice per plugin, a schema
// version embedded in the slice, and discard-and-rebuild instead of migration.
// The archive itself is described under "archive format" below.
// ---------------------------------------------------------------------------

/// Layout version stamped into every persisted slice.
///
/// A slice recording a different value is discarded rather than migrated: the
/// catalog is reconstructible from its plugins, so *discard and rebuild* is
/// both cheaper and safer than carrying a reader for every past layout
/// (ADR-0008). Bumping this constant therefore invalidates every cached slice.
pub const SCHEMA_VERSION: u32 = 1;

/// Why a persistent cache operation could not be carried out.
///
/// Damage is deliberately absent from this enum. A slice that cannot be
/// trusted is a miss, not a failure: startup stage 2 must rebuild it, and a
/// launcher that refuses to start because a cached file was torn by a power
/// cut is worse than one that spends a second re-indexing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    /// The cache root or one of its files could not be accessed.
    Io {
        /// The path the failing operation named.
        path: PathBuf,
        /// The failure reported by the operating system.
        kind: io::ErrorKind,
    },
    /// The plugin id does not fit in one path component once escaped, so no
    /// file could represent it.
    UnsupportedPluginId {
        /// The owner that cannot be given a file name.
        plugin: PluginId,
        /// Bytes the escaped name would occupy.
        encoded_bytes: usize,
        /// Bytes a single path component may occupy.
        limit: usize,
    },
    /// A slice contains an item owned by a different plugin and cannot be
    /// persisted as one owner's archive.
    InvalidSliceOwner {
        /// The owner named by the invalid slice.
        plugin: PluginId,
    },
    /// The encoded slice exceeds either a length-prefix or the bounded archive
    /// size, so storing it would create a cache entry this reader must reject.
    SliceTooLarge {
        /// The owner whose slice could not be encoded.
        plugin: PluginId,
    },
}

impl CacheError {
    fn io(path: &Path, error: &io::Error) -> Self {
        CacheError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        }
    }
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Io { path, kind } => write!(
                f,
                "catalog cache i/o failed at {path}: {kind:?}",
                path = path.display()
            ),
            CacheError::UnsupportedPluginId {
                plugin,
                encoded_bytes,
                limit,
            } => write!(
                f,
                "plugin id {id:?} escapes to {encoded_bytes} bytes, past the {limit} byte file name limit",
                id = plugin.0
            ),
            CacheError::InvalidSliceOwner { plugin } => write!(
                f,
                "catalog slice for {id:?} contains an item owned by another plugin",
                id = plugin.0
            ),
            CacheError::SliceTooLarge { plugin } => write!(
                f,
                "a field of the catalog slice owned by {id:?} exceeds the archive format's limits",
                id = plugin.0
            ),
        }
    }
}

impl std::error::Error for CacheError {}

/// One plugin's persisted catalog contribution.
///
/// Slices are per plugin so a rebuild, a crashed worker or a damaged archive
/// only ever costs that plugin's items (ADR-0008).
#[derive(Debug, Clone)]
pub struct CachedSlice {
    /// Owner of every item in the slice.
    pub plugin: PluginId,
    /// The plugin instance that published these items, so a restarted worker
    /// cannot be mistaken for the one whose bytes are on disk.
    pub instance: u64,
    /// Generation the slice was published under.
    pub generation: Generation,
    /// Items in the owner's stable catalog order.
    pub items: Vec<Item>,
}

/// Persistent cache of the core catalog, loaded during stage 2 of startup.
///
/// Object safe on purpose: startup hands the search service a
/// `&dyn CatalogCache` so a test double and the file backed cache are
/// interchangeable.
pub trait CatalogCache {
    /// Reads a plugin's slice.
    ///
    /// Returns `Ok(None)` when nothing trustworthy is stored: an absent cache
    /// root, an owner that was never cached, or an archive that is corrupt,
    /// truncated, written by a foreign schema version or fails its checksum.
    /// `Err` is reserved for a filesystem fault that says nothing about the
    /// slice itself.
    fn load_slice(&self, plugin: &PluginId) -> Result<Option<CachedSlice>, CacheError>;

    /// Replaces a plugin's slice atomically: a reader sees either the whole
    /// previous slice or the whole new one, never a mixture.
    fn store_slice(&self, slice: &CachedSlice) -> Result<(), CacheError>;

    /// Drops a plugin's slice (spec 22.4).
    ///
    /// Idempotent, and an owner that was never cached is not an error: the
    /// rebuild path invalidates before it knows whether anything was stored.
    fn invalidate(&self, plugin: &PluginId) -> Result<(), CacheError>;

    /// Owners the cache holds bytes for, sorted and free of duplicates.
    fn plugins(&self) -> Result<Vec<PluginId>, CacheError>;
}

// ---------------------------------------------------------------------------
// archive format
// ---------------------------------------------------------------------------
//
// A slice is one file:
//
//     0..8    magic
//     8..12   SCHEMA_VERSION, little endian u32
//     12..20  payload length, little endian u64
//     20..28  payload checksum, little endian u64
//     28..    payload
//
// The version sits in the header so a foreign layout is rejected before a
// single payload byte is interpreted, and the checksum covers the payload so a
// flipped bit that still parses cannot reach the launcher as catalog text.
//
// Loading is a full decode into owned values, never a view over the file: the
// bytes are read, checked and parsed field by field into `String`s and `Vec`s,
// and dropped when `decode_archive` returns. The layout is not built to be
// interpreted in place — every field is length prefixed and unaligned, so a
// reader could not borrow one without copying it anyway. What the parse buys
// is a decoder that rejects a damaged or hostile archive field by field and
// hands the launcher only values it has already validated.

const MAGIC_BYTES: usize = 8;
const MAGIC: [u8; MAGIC_BYTES] = *b"CRIKYCAT";
/// Largest archive the file-backed cache will read into memory.
///
/// Catalog slices are rebuildable, so a file larger than this bound is a
/// hostile or foreign cache entry rather than a reason to risk an unbounded
/// allocation during startup. The limit leaves room for the measured
/// 500,000-item archive in ADR-0008 while keeping damaged-file reads bounded.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const VERSION_BYTES: usize = 4;
const PAYLOAD_LEN_BYTES: usize = 8;
const CHECKSUM_BYTES: usize = 8;
const HEADER_BYTES: usize = MAGIC_BYTES + VERSION_BYTES + PAYLOAD_LEN_BYTES + CHECKSUM_BYTES;
const PAYLOAD_LEN_OFFSET: usize = MAGIC_BYTES + VERSION_BYTES;
const CHECKSUM_OFFSET: usize = PAYLOAD_LEN_OFFSET + PAYLOAD_LEN_BYTES;

// Payload tags. These are the wire format: renumbering one is a schema change.
const CATEGORY_APPLICATION: u8 = 0;
const CATEGORY_FILE: u8 = 1;
const CATEGORY_DIRECTORY: u8 = 2;
const CATEGORY_URL: u8 = 3;
const CATEGORY_COMMAND: u8 = 4;
const CATEGORY_EXPRESSION: u8 = 5;
const CATEGORY_KEYWORD: u8 = 6;
const CATEGORY_CONTACT: u8 = 7;
const CATEGORY_CLIPBOARD_ITEM: u8 = 8;
const CATEGORY_PLUGIN_DEFINED: u8 = 9;
const ARGUMENT_FORBIDDEN: u8 = 0;
const ARGUMENT_OPTIONAL: u8 = 1;
const ARGUMENT_REQUIRED: u8 = 2;
const HIT_RECORDED: u8 = 0;
const HIT_IGNORED: u8 = 1;
const EXECUTION_HOST_MEDIATED: u8 = 0;
const EXECUTION_PLUGIN: u8 = 1;
const OPTION_NONE: u8 = 0;
const OPTION_SOME: u8 = 1;

// Smallest encoding of each repeated element. Paired with the element's
// decoded size in `bounded_capacity`, this is what keeps a corrupt count from
// reserving more memory than the archive that named it.
const MIN_STRING_BYTES: usize = 4;
const MIN_CATEGORY_BYTES: usize = 1;
const MIN_ACTION_BYTES: usize = 18;
const MIN_ITEM_BYTES: usize = 40;

/// Buffer size hint per item. A wrong guess costs a reallocation, never
/// correctness.
const ESTIMATED_ITEM_BYTES: usize = 64;

/// A value whose encoded length overflows the archive's 32 bit length prefix.
#[derive(Debug)]
struct FieldTooLarge;

type Encoded = Result<(), FieldTooLarge>;

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_count(out: &mut Vec<u8>, count: usize) -> Encoded {
    put_u32(out, u32::try_from(count).map_err(|_| FieldTooLarge)?);
    Ok(())
}

fn put_str(out: &mut Vec<u8>, value: &str) -> Encoded {
    put_count(out, value.len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_optional_str(out: &mut Vec<u8>, value: Option<&str>) -> Encoded {
    match value {
        None => {
            out.push(OPTION_NONE);
            Ok(())
        }
        Some(value) => {
            out.push(OPTION_SOME);
            put_str(out, value)
        }
    }
}

fn put_category(out: &mut Vec<u8>, category: &Category) -> Encoded {
    let tag = match category {
        Category::Application => CATEGORY_APPLICATION,
        Category::File => CATEGORY_FILE,
        Category::Directory => CATEGORY_DIRECTORY,
        Category::Url => CATEGORY_URL,
        Category::Command => CATEGORY_COMMAND,
        Category::Expression => CATEGORY_EXPRESSION,
        Category::Keyword => CATEGORY_KEYWORD,
        Category::Contact => CATEGORY_CONTACT,
        Category::ClipboardItem => CATEGORY_CLIPBOARD_ITEM,
        Category::PluginDefined(_) => CATEGORY_PLUGIN_DEFINED,
    };
    out.push(tag);
    match category {
        Category::PluginDefined(name) => put_str(out, name),
        _ => Ok(()),
    }
}

fn put_action(out: &mut Vec<u8>, action: &Action) -> Encoded {
    put_str(out, &action.action_id.0)?;
    put_str(out, &action.label)?;
    put_str(out, &action.description)?;
    put_count(out, action.applicable_categories.len())?;
    for category in &action.applicable_categories {
        put_category(out, category)?;
    }
    put_optional_str(out, action.icon_reference.as_deref())?;
    out.push(match action.execution_policy {
        ExecutionPolicy::HostMediated => EXECUTION_HOST_MEDIATED,
        ExecutionPolicy::Plugin => EXECUTION_PLUGIN,
    });
    Ok(())
}

fn put_item(out: &mut Vec<u8>, item: &Item) -> Encoded {
    put_str(out, &item.stable_id.0)?;
    put_str(out, &item.plugin_id.0)?;
    put_category(out, &item.category)?;
    put_str(out, &item.label)?;
    put_str(out, &item.description)?;
    put_str(out, &item.target)?;
    put_count(out, item.search_terms.len())?;
    for term in &item.search_terms {
        put_str(out, term)?;
    }
    put_optional_str(out, item.icon_reference.as_deref())?;
    out.push(match item.argument_policy {
        ArgumentPolicy::Forbidden => ARGUMENT_FORBIDDEN,
        ArgumentPolicy::Optional => ARGUMENT_OPTIONAL,
        ArgumentPolicy::Required => ARGUMENT_REQUIRED,
    });
    out.push(match item.hit_policy {
        HitPolicy::Recorded => HIT_RECORDED,
        HitPolicy::Ignored => HIT_IGNORED,
    });
    out.extend_from_slice(&item.score_hint.to_le_bytes());
    put_count(out, item.metadata.len())?;
    for (key, value) in &item.metadata {
        put_str(out, key)?;
        put_str(out, value)?;
    }
    put_count(out, item.actions.len())?;
    for action in &item.actions {
        put_action(out, action)?;
    }
    Ok(())
}

/// Encodes a slice as a complete archive, header included.
///
/// The payload is built straight into the output buffer and the two header
/// fields that depend on it are backfilled, so a large catalog is never held
/// twice in memory.
fn encode_archive(slice: &CachedSlice) -> Result<Vec<u8>, FieldTooLarge> {
    let hint = HEADER_BYTES
        .saturating_add(slice.plugin.0.len())
        .saturating_add(slice.items.len().saturating_mul(ESTIMATED_ITEM_BYTES));
    let mut archive = Vec::with_capacity(hint);

    archive.extend_from_slice(&MAGIC);
    archive.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    archive.extend_from_slice(&[0u8; PAYLOAD_LEN_BYTES + CHECKSUM_BYTES]);

    put_str(&mut archive, &slice.plugin.0)?;
    put_u64(&mut archive, slice.instance);
    put_u64(&mut archive, slice.generation.get());
    put_count(&mut archive, slice.items.len())?;
    for item in &slice.items {
        put_item(&mut archive, item)?;
    }

    let payload = archive.get(HEADER_BYTES..).unwrap_or_default();
    let payload_len = (payload.len() as u64).to_le_bytes();
    let digest = checksum(payload).to_le_bytes();
    if let Some(field) = archive.get_mut(PAYLOAD_LEN_OFFSET..PAYLOAD_LEN_OFFSET + PAYLOAD_LEN_BYTES) {
        field.copy_from_slice(&payload_len);
    }
    if let Some(field) = archive.get_mut(CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_BYTES) {
        field.copy_from_slice(&digest);
    }
    Ok(archive)
}

/// 64 bit FNV-1a folded a word at a time, with a rotation so a bit flipped in
/// a late word still reaches the low bits of the digest.
///
/// Hand rolled deliberately: the workspace takes no third party dependency for
/// a checksum whose only job is to turn a silently damaged slice into a miss.
fn checksum(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    const WORD: usize = 8;

    fn word_of(chunk: &[u8]) -> u64 {
        let mut buffer = [0u8; WORD];
        if let Some(slot) = buffer.get_mut(..chunk.len()) {
            slot.copy_from_slice(chunk);
        }
        u64::from_le_bytes(buffer)
    }

    let mut digest = OFFSET_BASIS;
    let mut words = bytes.chunks_exact(WORD);
    for chunk in words.by_ref() {
        digest = (digest ^ word_of(chunk)).wrapping_mul(PRIME).rotate_left(27);
    }
    digest = (digest ^ word_of(words.remainder())).wrapping_mul(PRIME);

    // The length is mixed in last so a truncation landing on a word boundary
    // cannot reproduce a shorter payload's digest.
    digest = (digest ^ bytes.len() as u64).wrapping_mul(PRIME);
    digest ^ (digest >> 32)
}

/// A bounds checked cursor over archive bytes.
///
/// Every read is fallible and nothing is ever indexed directly: returning
/// `None` is how hostile or damaged bytes become a cache miss instead of a
/// panic in startup stage 2.
#[derive(Debug)]
struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn remaining(&self) -> usize {
        self.bytes.len()
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        // Copied out of `self` first so the returned slice borrows the archive
        // for `'a` rather than for the life of this `&mut self`.
        let bytes: &'a [u8] = self.bytes;
        let (head, tail) = bytes.split_at_checked(len)?;
        self.bytes = tail;
        Some(head)
    }

    fn take_array<const N: usize>(&mut self) -> Option<[u8; N]> {
        <[u8; N]>::try_from(self.take(N)?).ok()
    }

    fn tag(&mut self) -> Option<u8> {
        let bytes: &'a [u8] = self.bytes;
        let (first, rest) = bytes.split_first()?;
        self.bytes = rest;
        Some(*first)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take_array()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take_array()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take_array()?))
    }

    fn count(&mut self) -> Option<usize> {
        usize::try_from(self.u32()?).ok()
    }

    fn string(&mut self) -> Option<String> {
        let len = self.count()?;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }

    fn optional_string(&mut self) -> Option<Option<String>> {
        match self.tag()? {
            OPTION_NONE => Some(None),
            OPTION_SOME => Some(Some(self.string()?)),
            _ => None,
        }
    }
}

/// Capacity for a decoded collection, clamped so an untrusted count cannot buy
/// an allocation out of proportion to the archive that asked for it.
///
/// Two ceilings apply: the elements the remaining bytes could still describe,
/// and the decoded footprint those same bytes can justify. The first alone is
/// not a bound worth having, because an element is routinely wider in memory
/// than on the wire — a `Category` costs one tag byte encoded and
/// `size_of::<Category>()` in a vector, so remaining-bytes clamping still lets
/// a kilobyte of hostile payload reserve tens of kilobytes, and a vector of
/// `String` several times its archive. The second ceiling caps the reservation
/// at the archive's own size, so the worst a lying count can cost is bounded
/// by the bytes someone had to write to tell the lie.
///
/// Under-reserving is harmless: a count too large for the ceiling but honest
/// only costs the growth path a reallocation, and any archive whose elements
/// encode to at least their decoded size — which is every realistic catalog —
/// still reserves its collections exactly once.
fn bounded_capacity<T>(count: usize, min_element_bytes: usize, remaining: usize) -> usize {
    let describable = remaining / min_element_bytes.max(1);
    let affordable = remaining / std::mem::size_of::<T>().max(1);
    count.min(describable).min(affordable)
}

fn take_category(cursor: &mut Cursor<'_>) -> Option<Category> {
    Some(match cursor.tag()? {
        CATEGORY_APPLICATION => Category::Application,
        CATEGORY_FILE => Category::File,
        CATEGORY_DIRECTORY => Category::Directory,
        CATEGORY_URL => Category::Url,
        CATEGORY_COMMAND => Category::Command,
        CATEGORY_EXPRESSION => Category::Expression,
        CATEGORY_KEYWORD => Category::Keyword,
        CATEGORY_CONTACT => Category::Contact,
        CATEGORY_CLIPBOARD_ITEM => Category::ClipboardItem,
        CATEGORY_PLUGIN_DEFINED => Category::PluginDefined(cursor.string()?),
        _ => return None,
    })
}

fn take_action(cursor: &mut Cursor<'_>) -> Option<Action> {
    let action_id = ActionId(cursor.string()?);
    let label = cursor.string()?;
    let description = cursor.string()?;

    let category_count = cursor.count()?;
    let capacity = bounded_capacity::<Category>(category_count, MIN_CATEGORY_BYTES, cursor.remaining());
    let mut applicable_categories = Vec::with_capacity(capacity);
    for _ in 0..category_count {
        applicable_categories.push(take_category(cursor)?);
    }

    let icon_reference = cursor.optional_string()?;
    let execution_policy = match cursor.tag()? {
        EXECUTION_HOST_MEDIATED => ExecutionPolicy::HostMediated,
        EXECUTION_PLUGIN => ExecutionPolicy::Plugin,
        _ => return None,
    };

    Some(Action {
        action_id,
        label,
        description,
        applicable_categories,
        icon_reference,
        execution_policy,
    })
}

fn take_item(cursor: &mut Cursor<'_>) -> Option<Item> {
    let stable_id = ItemId(cursor.string()?);
    let plugin_id = PluginId(cursor.string()?);
    let category = take_category(cursor)?;
    let label = cursor.string()?;
    let description = cursor.string()?;
    let target = cursor.string()?;

    let term_count = cursor.count()?;
    let capacity = bounded_capacity::<String>(term_count, MIN_STRING_BYTES, cursor.remaining());
    let mut search_terms = Vec::with_capacity(capacity);
    for _ in 0..term_count {
        search_terms.push(cursor.string()?);
    }

    let icon_reference = cursor.optional_string()?;
    let argument_policy = match cursor.tag()? {
        ARGUMENT_FORBIDDEN => ArgumentPolicy::Forbidden,
        ARGUMENT_OPTIONAL => ArgumentPolicy::Optional,
        ARGUMENT_REQUIRED => ArgumentPolicy::Required,
        _ => return None,
    };
    let hit_policy = match cursor.tag()? {
        HIT_RECORDED => HitPolicy::Recorded,
        HIT_IGNORED => HitPolicy::Ignored,
        _ => return None,
    };
    let score_hint = cursor.i32()?;

    let metadata_count = cursor.count()?;
    let mut metadata = BTreeMap::new();
    for _ in 0..metadata_count {
        let key = cursor.string()?;
        let value = cursor.string()?;
        metadata.insert(key, value);
    }
    if metadata.len() != metadata_count {
        // A duplicate key means these bytes were not produced by this codec.
        return None;
    }

    let action_count = cursor.count()?;
    let capacity = bounded_capacity::<Action>(action_count, MIN_ACTION_BYTES, cursor.remaining());
    let mut actions = Vec::with_capacity(capacity);
    for _ in 0..action_count {
        actions.push(take_action(cursor)?);
    }

    Some(Item {
        stable_id,
        plugin_id,
        category,
        label,
        description,
        target,
        search_terms,
        icon_reference,
        argument_policy,
        hit_policy,
        score_hint,
        metadata,
        actions,
    })
}

/// Encodes one slice as a self-contained slice document.
///
/// Public because the archive is not only a cache entry: a remote catalog
/// source publishes exactly these bytes over the network, and the launcher
/// admits them through the same bounded, field-by-field decoder it uses for a
/// file it wrote itself (ADR-0016). Exposing the encoder is what lets a
/// publisher produce a document this decoder accepts without a second format
/// and a second set of validation rules.
///
/// `None` means one field's encoded length overflows the archive's 32 bit
/// length prefix, which no realistic catalog reaches.
pub fn encode_slice_document(slice: &CachedSlice) -> Option<Vec<u8>> {
    encode_archive(slice).ok()
}

/// Decodes a slice document whose owner is stated by the document itself.
///
/// `None` covers every reason the bytes cannot be trusted: wrong magic, a
/// foreign schema version, a length that disagrees with the document, a
/// checksum mismatch, an unknown tag, invalid UTF-8, a short read, trailing
/// bytes, or an item claiming an owner other than the one the document
/// records.
///
/// The owner is *read* rather than *asserted* here, which is the one thing a
/// remote document needs that a cache entry does not: the publisher names
/// itself, and the caller decides whether that name is one it will admit.
pub fn decode_slice_document(bytes: &[u8]) -> Option<CachedSlice> {
    let mut header = Cursor::new(bytes);
    if header.take_array::<MAGIC_BYTES>()? != MAGIC {
        return None;
    }
    if header.u32()? != SCHEMA_VERSION {
        return None;
    }
    let payload_len = usize::try_from(header.u64()?).ok()?;
    let recorded = header.u64()?;
    let payload = header.take(payload_len)?;
    if !header.is_empty() || checksum(payload) != recorded {
        return None;
    }

    let mut cursor = Cursor::new(payload);
    let owner = PluginId(cursor.string()?);
    let instance = cursor.u64()?;
    let generation = Generation::from_raw(cursor.u64()?);

    let item_count = cursor.count()?;
    let capacity = bounded_capacity::<Item>(item_count, MIN_ITEM_BYTES, cursor.remaining());
    let mut items = Vec::with_capacity(capacity);
    for _ in 0..item_count {
        let item = take_item(&mut cursor)?;
        // A slice may only hand back items owned by the plugin it was loaded
        // for. Publishing a foreign item would be rejected by the in-memory
        // catalog, turning a tampered archive into a startup failure instead of
        // the rebuild ADR-0008 asks for.
        if item.plugin_id != owner {
            return None;
        }
        items.push(item);
    }
    if !cursor.is_empty() {
        return None;
    }

    Some(CachedSlice {
        plugin: owner,
        instance,
        generation,
        items,
    })
}

/// Decodes an archive that must belong to `plugin`.
///
/// Everything [`decode_slice_document`] refuses, plus an archive recording a
/// different owner: a cache entry is read from the file named after its owner,
/// so the name and the contents must agree.
fn decode_archive(plugin: &PluginId, bytes: &[u8]) -> Option<CachedSlice> {
    decode_slice_document(bytes).filter(|slice| &slice.plugin == plugin)
}

// ---------------------------------------------------------------------------
// file naming
// ---------------------------------------------------------------------------

const SLICE_SUFFIX: &str = ".slice";
const TEMP_PREFIX: &str = "tmp-";
const TEMP_SUFFIX: &str = ".part";
const ESCAPE: u8 = b'%';
const HEX_DIGITS: [u8; 16] = *b"0123456789abcdef";

/// The longest single path component mainstream filesystems accept.
const MAX_FILE_NAME_BYTES: usize = 255;

/// Bytes a slice file name may carry literally.
///
/// Uppercase letters are excluded on purpose: plugin ids are exact and case
/// sensitive, so `Widget` and `widget` must stay distinct files even on a case
/// insensitive filesystem.
fn is_literal(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
}
fn is_windows_reserved_id(id: &str) -> bool {
    let stem = id
        .split_once('.')
        .map_or(id, |(stem, _)| stem)
        .trim_end_matches(['.', ' ']);
    matches!(
        stem,
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    )
}

/// Escapes a plugin id into one path-safe component.
///
/// Plugin ids are arbitrary manifest strings, so every byte outside the
/// literal set becomes `%xx`. Windows device names receive one additional
/// escaped leading byte, because names such as `con.slice` are reserved even
/// when they have an extension. The mapping remains injective, which is what
/// lets [`plugins`] recover exact owner ids from file names alone.
///
/// [`plugins`]: CatalogCache::plugins
fn escape_plugin_id(plugin: &PluginId) -> String {
    let id = plugin.0.as_bytes();
    let reserved = is_windows_reserved_id(&plugin.0);
    let mut name = String::with_capacity(id.len().saturating_add(SLICE_SUFFIX.len()));
    for (index, &byte) in id.iter().enumerate() {
        if is_literal(byte) && !(reserved && index == 0) {
            name.push(char::from(byte));
        } else {
            name.push(char::from(ESCAPE));
            name.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            name.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
        }
    }
    name.push_str(SLICE_SUFFIX);
    name
}

/// The file name a plugin's slice occupies, or `None` when its escaped id
/// cannot fit in one path component.
fn slice_file_name(plugin: &PluginId) -> Option<String> {
    let name = escape_plugin_id(plugin);
    (name.len() <= MAX_FILE_NAME_BYTES).then_some(name)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// The owner of a slice file, or `None` for any name this cache did not write.
///
/// Only the canonical escaping is accepted, so a foreign file dropped into the
/// cache root is ignored rather than reported as an owner.
fn plugin_from_file_name(name: &str) -> Option<PluginId> {
    let stem = name.strip_suffix(SLICE_SUFFIX)?;
    let mut id = Vec::with_capacity(stem.len());
    let mut bytes = stem.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == ESCAPE {
            let high = hex_value(bytes.next()?)?;
            let low = hex_value(bytes.next()?)?;
            id.push((high << 4) | low);
        } else if is_literal(byte) {
            id.push(byte);
        } else {
            return None;
        }
    }
    let plugin = PluginId(String::from_utf8(id).ok()?);
    (escape_plugin_id(&plugin) == name).then_some(plugin)
}

static NEXT_TEMP_TICKET: AtomicU64 = AtomicU64::new(0);

/// A scratch name unique to this process and this call, so two writers never
/// share the file that is about to be renamed into place.
///
/// It is deliberately independent of the plugin id: an already long escaped id
/// must not overflow the file name limit only while being written.
fn temp_file_name() -> String {
    let ticket = NEXT_TEMP_TICKET.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("{TEMP_PREFIX}{pid}-{ticket}{TEMP_SUFFIX}")
}

/// Abandoned scratch files are safe to reclaim after this generous interval:
/// a live write is bounded by the archive limit and should finish well before
/// one hour, while a crash leaves the file permanently unchanged.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Best-effort cleanup for scratch files left by an interrupted writer.
///
/// Errors are ignored deliberately. Reclamation must never turn a valid cache
/// write into a cache failure, and the age check keeps a live writer's file
/// out of the normal path.
fn remove_stale_temps(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(TEMP_PREFIX) || !name.ends_with(TEMP_SUFFIX) {
            continue;
        }
        let Ok(modified) = fs::symlink_metadata(&path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age >= STALE_TEMP_AGE {
            let _ = fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------------------------
// file backed cache
// ---------------------------------------------------------------------------

/// A [`CatalogCache`] holding one archive per plugin under a single root.
///
/// Durability is intentionally not pursued: the cache is fully reconstructible
/// from the plugins, and a slice torn by a crash is detected by its checksum
/// and rebuilt, so paying an `fsync` per store would buy nothing a rebuild does
/// not already give.
#[derive(Debug, Clone)]
pub struct FileCatalogCache {
    root: PathBuf,
}

impl FileCatalogCache {
    /// Binds a cache to `root` without touching the filesystem.
    ///
    /// Startup builds the handle before it knows whether a cache exists, and an
    /// absent root is a cold cache rather than a fault, so creating directories
    /// here would turn a first launch into an I/O failure path.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The directory this cache stores slices in.
    pub fn root(&self) -> &Path {
        &self.root
    }
}
/// Reads one archive without allowing its on-disk length to control an
/// unbounded allocation.
///
/// The extra byte read detects a file that grows after metadata inspection.
/// Such a race is a cache miss just like an archive that was already too large;
/// neither case is allowed to reach the decoder.
fn read_archive(path: &Path) -> Result<Option<Vec<u8>>, CacheError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CacheError::io(path, &error)),
    };
    let length = file
        .metadata()
        .map_err(|error| CacheError::io(path, &error))?
        .len();
    if length > MAX_ARCHIVE_BYTES {
        return Ok(None);
    }

    let capacity = usize::try_from(length).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    let mut limited = file.take(MAX_ARCHIVE_BYTES.saturating_add(1));
    limited
        .read_to_end(&mut bytes)
        .map_err(|error| CacheError::io(path, &error))?;
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Ok(None);
    }
    Ok(Some(bytes))
}

impl CatalogCache for FileCatalogCache {
    fn load_slice(&self, plugin: &PluginId) -> Result<Option<CachedSlice>, CacheError> {
        // An id with no representable file name can never have been stored.
        let Some(file_name) = slice_file_name(plugin) else {
            return Ok(None);
        };

        let path = self.root.join(file_name);
        let Some(bytes) = read_archive(&path)? else {
            return Ok(None);
        };

        Ok(decode_archive(plugin, &bytes))
    }

    fn store_slice(&self, slice: &CachedSlice) -> Result<(), CacheError> {
        let Some(file_name) = slice_file_name(&slice.plugin) else {
            return Err(CacheError::UnsupportedPluginId {
                plugin: slice.plugin.clone(),
                encoded_bytes: escape_plugin_id(&slice.plugin).len(),
                limit: MAX_FILE_NAME_BYTES,
            });
        };
        if slice.items.iter().any(|item| item.plugin_id != slice.plugin) {
            return Err(CacheError::InvalidSliceOwner {
                plugin: slice.plugin.clone(),
            });
        }
        let archive = encode_archive(slice).map_err(|_| CacheError::SliceTooLarge {
            plugin: slice.plugin.clone(),
        })?;
        if archive.len() as u64 > MAX_ARCHIVE_BYTES {
            return Err(CacheError::SliceTooLarge {
                plugin: slice.plugin.clone(),
            });
        }

        fs::create_dir_all(&self.root).map_err(|error| CacheError::io(&self.root, &error))?;
        remove_stale_temps(&self.root);
        // Exclusive creation prevents a stale scratch name or a symlink from
        // redirecting the archive write outside this cache root.

        let temp = loop {
            let candidate = self.root.join(temp_file_name());
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(&archive) {
                        let _ = fs::remove_file(&candidate);
                        return Err(CacheError::io(&candidate, &error));
                    }
                    break candidate;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(CacheError::io(&candidate, &error)),
            }
        };
        let path = self.root.join(file_name);
        if let Err(error) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            return Err(CacheError::io(&path, &error));
        }
        Ok(())
    }

    fn invalidate(&self, plugin: &PluginId) -> Result<(), CacheError> {
        let Some(file_name) = slice_file_name(plugin) else {
            return Ok(());
        };

        let path = self.root.join(file_name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CacheError::io(&path, &error)),
        }
    }

    fn plugins(&self) -> Result<Vec<PluginId>, CacheError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(CacheError::io(&self.root, &error)),
        };

        let mut plugins = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| CacheError::io(&self.root, &error))?;
            let Some(owner) = entry.file_name().to_str().and_then(plugin_from_file_name) else {
                continue;
            };
            let file_type = entry
                .file_type()
                .map_err(|error| CacheError::io(&entry.path(), &error))?;
            if !file_type.is_file() {
                continue;
            }
            plugins.push(owner);
        }

        // Directory order is not defined; owners are reported sorted so callers
        // see the same catalog on every launch.
        plugins.sort();
        Ok(plugins)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest element count a `u32` length prefix can carry: the worst a
    /// damaged or hostile archive can ask a decoder to reserve for.
    const HOSTILE_COUNT: usize = u32::MAX as usize;

    /// Archive sizes from "smaller than one element" up to a megabyte.
    const ARCHIVE_SIZES: [usize; 6] = [0, 1, 7, 64, 4_096, 1 << 20];

    /// Markers placed in fields the payload encodes immediately before a
    /// count, so a count can be located without hard coding an offset.
    const TARGET_MARKER: &str = "hostile-count-target";
    const DESCRIPTION_MARKER: &str = "hostile-count-description";

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    /// One item carrying one action, each with an empty collection right after
    /// a marker field: the search terms follow the target, the applicable
    /// categories follow the action description.
    fn hostile_fixture(plugin: &PluginId) -> CachedSlice {
        let action = Action {
            action_id: ActionId("hostile.action".to_owned()),
            label: String::new(),
            description: DESCRIPTION_MARKER.to_owned(),
            applicable_categories: Vec::new(),
            icon_reference: None,
            execution_policy: ExecutionPolicy::HostMediated,
        };
        let item = Item {
            stable_id: ItemId("hostile.item".to_owned()),
            plugin_id: plugin.clone(),
            category: Category::File,
            label: String::new(),
            description: String::new(),
            target: TARGET_MARKER.to_owned(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: vec![action],
        };
        CachedSlice {
            plugin: plugin.clone(),
            instance: 7,
            generation: Generation::from_raw(3),
            items: vec![item],
        }
    }

    /// Restamps the payload checksum so a patched archive reaches the field
    /// decoding this module is being tested on, instead of being turned away
    /// before a single count is read.
    fn restamp(archive: &mut [u8]) {
        let payload = archive
            .get(HEADER_BYTES..)
            .expect("an encoded archive carries a header");
        let digest = checksum(payload).to_le_bytes();
        archive
            .get_mut(CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_BYTES)
            .expect("an encoded archive carries a checksum field")
            .copy_from_slice(&digest);
    }

    /// Rewrites the count that follows `marker`, after checking it reads as
    /// `expected`: a layout probe that fails loudly beats silently damaging an
    /// unrelated field and calling the resulting miss a pass.
    fn patch_count(archive: &[u8], marker: &str, expected: u32, hostile: u32) -> Vec<u8> {
        let at = find(archive, marker.as_bytes()).expect("the marker field must reach the payload verbatim")
            + marker.len();
        assert_eq!(
            archive.get(at..at + 4),
            Some(expected.to_le_bytes().as_slice()),
            "layout: an element count of {expected} must follow {marker:?} in the payload"
        );

        let mut patched = archive.to_vec();
        patched[at..at + 4].copy_from_slice(&hostile.to_le_bytes());
        restamp(&mut patched);
        patched
    }

    fn assert_hostile_count_is_contained<T>(
        plugin: &PluginId,
        archive: &[u8],
        field: &str,
        marker: &str,
        min_element_bytes: usize,
    ) {
        for hostile in [1u32 << 24, u32::MAX / 2, u32::MAX] {
            let patched = patch_count(archive, marker, 0, hostile);
            assert!(
                decode_archive(plugin, &patched).is_none(),
                "a {field} count of {hostile} the payload cannot honour must read as a cache miss"
            );

            // Measured against the whole archive, which is more than the
            // decoder has left when it reaches this count: even that generous
            // bound may not be exceeded.
            let reserved = bounded_capacity::<T>(hostile as usize, min_element_bytes, patched.len())
                * std::mem::size_of::<T>();
            assert!(
                reserved <= patched.len(),
                "a {field} count of {hostile} would reserve {reserved} bytes from a {len} byte archive",
                len = patched.len()
            );
        }
    }

    #[test]
    fn a_hostile_count_reserves_no_more_than_the_archive_can_justify() {
        for remaining in ARCHIVE_SIZES {
            let reservations = [
                (
                    "category",
                    bounded_capacity::<Category>(HOSTILE_COUNT, MIN_CATEGORY_BYTES, remaining)
                        * std::mem::size_of::<Category>(),
                ),
                (
                    "search term",
                    bounded_capacity::<String>(HOSTILE_COUNT, MIN_STRING_BYTES, remaining)
                        * std::mem::size_of::<String>(),
                ),
                (
                    "action",
                    bounded_capacity::<Action>(HOSTILE_COUNT, MIN_ACTION_BYTES, remaining)
                        * std::mem::size_of::<Action>(),
                ),
                (
                    "item",
                    bounded_capacity::<Item>(HOSTILE_COUNT, MIN_ITEM_BYTES, remaining)
                        * std::mem::size_of::<Item>(),
                ),
            ];

            for (element, reserved) in reservations {
                assert!(
                    reserved <= remaining,
                    "a hostile {element} count reserved {reserved} bytes from a {remaining} byte archive"
                );
            }
        }
    }

    #[test]
    fn clamping_by_encoded_bytes_alone_would_not_bound_the_reservation() {
        // The ceiling this defends is the footprint one. A category costs a
        // single tag byte encoded and `size_of::<Category>()` in a vector, so
        // a count clamped only by the bytes that could still describe its
        // elements still buys an allocation many times the archive.
        let remaining = 4_096;
        let encoded_only = HOSTILE_COUNT.min(remaining / MIN_CATEGORY_BYTES);
        assert!(
            encoded_only * std::mem::size_of::<Category>() > remaining,
            "the fixture must describe an element that is wider decoded than encoded"
        );
        assert!(
            bounded_capacity::<Category>(HOSTILE_COUNT, MIN_CATEGORY_BYTES, remaining)
                * std::mem::size_of::<Category>()
                <= remaining,
            "a category count must not reserve past the archive that named it"
        );
    }

    #[test]
    fn an_honest_count_is_still_reserved_in_one_allocation() {
        // Every realistic catalog encodes an element in at least the bytes it
        // occupies decoded, so the clamp costs the 500,000 item target nothing.
        let count = 500;
        assert_eq!(
            bounded_capacity::<Item>(count, MIN_ITEM_BYTES, count * std::mem::size_of::<Item>()),
            count
        );
        assert_eq!(
            bounded_capacity::<Action>(count, MIN_ACTION_BYTES, count * std::mem::size_of::<Action>()),
            count
        );
        assert_eq!(
            bounded_capacity::<String>(count, MIN_STRING_BYTES, count * std::mem::size_of::<String>()),
            count
        );
        assert_eq!(
            bounded_capacity::<Category>(count, MIN_CATEGORY_BYTES, count * std::mem::size_of::<Category>()),
            count
        );
    }

    #[test]
    fn hostile_element_counts_decode_as_misses_without_an_outsized_reservation() {
        let plugin = PluginId("hostile.plugin".to_owned());
        let archive = encode_archive(&hostile_fixture(&plugin)).expect("the fixture must encode");
        assert!(
            decode_archive(&plugin, &archive).is_some(),
            "the fixture archive must decode before it is damaged"
        );

        assert_hostile_count_is_contained::<String>(
            &plugin,
            &archive,
            "search-term",
            TARGET_MARKER,
            MIN_STRING_BYTES,
        );
        assert_hostile_count_is_contained::<Category>(
            &plugin,
            &archive,
            "applicable-category",
            DESCRIPTION_MARKER,
            MIN_CATEGORY_BYTES,
        );
    }
}
