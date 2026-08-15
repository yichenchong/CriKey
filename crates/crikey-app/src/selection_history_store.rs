//! Durable selection history (spec 11.3).
//!
//! The ranker's history store is in-memory by design: it is the thing that
//! makes ranking reproducible from one generation to the next, so it must not
//! also own a file. That leaves somebody else to make the learning survive a
//! restart, because a launcher that forgets every selection when the user logs
//! out has learned nothing at all.
//!
//! This is that somebody, and it is deliberately the same shape as the startup
//! journal: a per-user state file read once during startup and rewritten after
//! a change. The two share `read_bounded` and the JSON `Cursor` rather than
//! growing a second, differently-buggy copy of no-follow opening, size
//! bounding and string escaping.
//!
//! # Why a damaged file is empty rather than fatal
//!
//! History is an optimisation. Losing it costs the user some ranking quality
//! until they select things again; refusing to start costs them the launcher.
//! So every failure this module can observe — absent, non-regular, oversized,
//! not UTF-8, not a record this version wrote — resolves to an empty history
//! and is repaired by the next [`SelectionHistoryStore::save`]. There is
//! exactly one failure the caller does see: a save that could not be written,
//! which is reported so the user learns their history is not being kept, and
//! which never stops the launch.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crikey_core::{ItemId, PluginId};
use crikey_ranking::{QueryAffinityRecord, SelectionHistorySnapshot, SelectionRecord};

use crate::startup_recovery::{read_bounded, write_json_string, Cursor};

/// Magic key and format version in one.
///
/// A record opens with `"crikey_selection_history": 1`, so the very first key
/// both identifies the file as this format's and pins the version. A future
/// version bumps the number and this parser rejects it — which loads as an
/// empty history, exactly as any other unreadable record does, rather than
/// reinterpreting fields whose meaning has changed.
const MAGIC_KEY: &str = "crikey_selection_history";

/// The format version this build writes and is the only one it accepts.
const FORMAT_VERSION: u32 = 1;

/// Distinguishes one save's staging file from every other save's in this
/// process; the pid distinguishes it from every other process's.
static SAVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Persists a [`SelectionHistorySnapshot`] at one path.
#[derive(Debug, Clone)]
pub struct SelectionHistoryStore {
    path: PathBuf,
}

impl SelectionHistoryStore {
    /// The largest history file accepted from disk.
    ///
    /// A record is one line per selected item plus one per (item, query) pair
    /// the user has ever confirmed. A person who has selected a few thousand
    /// distinct things through a few thousand distinct queries still fits well
    /// inside this, and anything past it was not written by CriKey. Reading it
    /// in full to find that out would let a hostile or accidentally huge file
    /// decide how much this process allocates before it has a window, and an
    /// allocator abort is not something a fallback can catch. Over-limit is
    /// therefore corruption, handled exactly as invalid bytes are.
    pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

    /// Binds the store to `path`. No IO happens here: a constructor that
    /// touched the disk would make "where does history live" a fallible
    /// question at a point in startup that has nowhere to report it.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the persisted history, or an empty one.
    ///
    /// Never fails; see the module documentation for why. A caller cannot
    /// distinguish "first launch" from "damaged record" here, and deliberately
    /// so: there is nothing different it could usefully do.
    pub fn load(&self) -> SelectionHistorySnapshot {
        read_bounded(&self.path, Self::MAX_BYTES)
            .and_then(|text| parse(&text))
            .unwrap_or_default()
    }

    /// Commits `snapshot` to the store's path.
    ///
    /// Written to a uniquely named sibling and renamed, for the reason
    /// [`StartupJournal::save`] does the same: a crash during the write must
    /// not replace a readable history with a truncated one, and a fixed
    /// `<file>.tmp` is shared by every process that ever saves, so two
    /// concurrent saves through one inode can publish a mixture of both
    /// records. Nothing here takes a lock, so uniqueness is the whole
    /// guarantee.
    ///
    /// # Errors
    ///
    /// Whatever the directory creation, write or rename reported. The caller
    /// reports it and carries on: an unwritable history costs the next launch
    /// its learning, never this one its startup.
    ///
    /// [`StartupJournal::save`]: crate::StartupJournal::save
    pub fn save(&self, snapshot: &SelectionHistorySnapshot) -> io::Result<()> {
        if let Some(parent) = self.path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }

        let mut staging = self.path.clone().into_os_string();
        staging.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            SAVE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let staging = PathBuf::from(staging);

        fs::write(&staging, serialize(&within_budget(snapshot, Self::MAX_BYTES)))?;
        match fs::rename(&staging, &self.path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&staging);
                Err(error)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------
//
// One JSON object, three keys, in a fixed order. The parser is strict for the
// same reason the journal's is: anything it does not recognize is corruption,
// and corruption loads as an empty history rather than as a guess at what the
// user once selected.

/// The most-used records of `snapshot` that fit in `budget` bytes.
///
/// The ranking store caps how many records it keeps and how large one affinity
/// key may be, so this should never have anything to do. It runs anyway,
/// because the two limits are set in a different crate from the one that owns
/// the file size, and the failure they guard against is not a truncated file
/// but a *silently empty* one: anything over
/// [`SelectionHistoryStore::MAX_BYTES`] loads as no history at all, discarding
/// everything the user ever taught the launcher.
///
/// Sizes are accumulated record by record rather than by serializing the whole
/// snapshot and measuring it. Measuring first would allocate the very thing
/// the budget exists to refuse; this way the peak is one record above budget.
fn within_budget(snapshot: &SelectionHistorySnapshot, budget: u64) -> SelectionHistorySnapshot {
    let mut used = envelope_bytes() as u64;
    let mut scratch = String::new();

    // Most-used first, then by key so that two hosts trimming the same history
    // keep the same records.
    let mut selections: Vec<&SelectionRecord> = snapshot.selections.iter().collect();
    selections.sort_by(|left, right| {
        right
            .frequency
            .cmp(&left.frequency)
            .then_with(|| (&left.plugin, &left.item).cmp(&(&right.plugin, &right.item)))
    });
    let mut affinities: Vec<&QueryAffinityRecord> = snapshot.query_affinities.iter().collect();
    affinities.sort_by(|left, right| {
        right.count.cmp(&left.count).then_with(|| {
            (&left.query, &left.plugin, &left.item).cmp(&(&right.query, &right.plugin, &right.item))
        })
    });

    let mut kept_selections = Vec::new();
    for record in selections {
        scratch.clear();
        push_selection(&mut scratch, record);
        let cost = scratch.len() as u64 + u64::from(!kept_selections.is_empty());
        if used.saturating_add(cost) > budget {
            break;
        }
        used += cost;
        kept_selections.push(record.clone());
    }

    let mut kept_affinities = Vec::new();
    for record in affinities {
        scratch.clear();
        push_affinity(&mut scratch, record);
        let cost = scratch.len() as u64 + u64::from(!kept_affinities.is_empty());
        if used.saturating_add(cost) > budget {
            break;
        }
        used += cost;
        kept_affinities.push(record.clone());
    }

    // Back into the store's own key order, so a file still round-trips into an
    // identically ordered snapshot.
    kept_selections.sort_by(|left, right| (&left.plugin, &left.item).cmp(&(&right.plugin, &right.item)));
    kept_affinities.sort_by(|left, right| {
        (&left.query, &left.plugin, &left.item).cmp(&(&right.query, &right.plugin, &right.item))
    });
    SelectionHistorySnapshot {
        selections: kept_selections,
        query_affinities: kept_affinities,
    }
}

/// Bytes the enclosing object costs with both arrays empty.
fn envelope_bytes() -> usize {
    serialize(&SelectionHistorySnapshot::default()).len()
}

fn serialize(snapshot: &SelectionHistorySnapshot) -> String {
    let mut out = String::new();
    out.push('{');
    write_json_string(&mut out, MAGIC_KEY);
    let _ = write!(out, ":{FORMAT_VERSION},\"selections\":[");
    for (index, record) in snapshot.selections.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_selection(&mut out, record);
    }
    out.push_str("],\"query_affinities\":[");
    for (index, record) in snapshot.query_affinities.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_affinity(&mut out, record);
    }
    out.push_str("]}");
    out
}

/// One selection record. Shared with the budget, so the size it measures is
/// the size the file will hold rather than an estimate that could drift.
fn push_selection(out: &mut String, record: &SelectionRecord) {
    out.push_str("{\"plugin\":");
    write_json_string(out, &record.plugin.0);
    out.push_str(",\"item\":");
    write_json_string(out, &record.item.0);
    let _ = write!(out, ",\"frequency\":{}", record.frequency);
    out.push_str(",\"last_selected_secs\":");
    match record.last_selected_secs {
        Some(secs) => {
            let _ = write!(out, "{secs}");
        }
        None => out.push_str("null"),
    }
    out.push('}');
}

/// One affinity record, on the same terms as [`push_selection`].
fn push_affinity(out: &mut String, record: &QueryAffinityRecord) {
    out.push_str("{\"plugin\":");
    write_json_string(out, &record.plugin.0);
    out.push_str(",\"item\":");
    write_json_string(out, &record.item.0);
    out.push_str(",\"query\":");
    write_json_string(out, &record.query);
    let _ = write!(out, ",\"count\":{}", record.count);
    out.push('}');
}

/// Parses a saved record, or `None` if the bytes are not one this build wrote.
fn parse(text: &str) -> Option<SelectionHistorySnapshot> {
    let mut cursor = Cursor::new(text);

    cursor.expect('{')?;
    // The magic must come first. Accepting it anywhere in the object would let
    // a file be half-parsed before its format was established.
    if cursor.string()? != MAGIC_KEY {
        return None;
    }
    cursor.expect(':')?;
    if cursor.number()? != FORMAT_VERSION {
        return None;
    }

    let mut selections = None;
    let mut query_affinities = None;
    while cursor.consume(',') {
        let key = cursor.string()?;
        cursor.expect(':')?;
        match key.as_str() {
            "selections" if selections.is_none() => selections = Some(parse_selections(&mut cursor)?),
            "query_affinities" if query_affinities.is_none() => {
                query_affinities = Some(parse_query_affinities(&mut cursor)?)
            }
            // An unknown or repeated key means these bytes were not written by
            // this version, and guessing at them would be inventing history.
            _ => return None,
        }
    }
    cursor.expect('}')?;
    cursor.skip_whitespace();
    if !cursor.rest().is_empty() {
        return None;
    }

    Some(SelectionHistorySnapshot {
        selections: selections?,
        query_affinities: query_affinities?,
    })
}

/// Reads the array of one-record-per-selected-item objects.
///
/// Written out rather than routed through a generic element callback: the
/// records differ in three of their four fields, so a shared walker would buy
/// nothing but a higher-ranked closure signature to get wrong.
fn parse_selections(cursor: &mut Cursor<'_>) -> Option<Vec<SelectionRecord>> {
    cursor.expect('[')?;
    let mut records = Vec::new();
    if cursor.consume(']') {
        return Some(records);
    }
    loop {
        records.push(parse_selection(cursor)?);
        if !cursor.consume(',') {
            break;
        }
    }
    cursor.expect(']')?;
    Some(records)
}

fn parse_selection(cursor: &mut Cursor<'_>) -> Option<SelectionRecord> {
    let mut plugin = None;
    let mut item = None;
    let mut frequency = None;
    let mut last_selected_secs = None;

    cursor.expect('{')?;
    loop {
        let key = cursor.string()?;
        cursor.expect(':')?;
        match key.as_str() {
            "plugin" if plugin.is_none() => plugin = Some(PluginId(cursor.string()?)),
            "item" if item.is_none() => item = Some(ItemId(cursor.string()?)),
            "frequency" if frequency.is_none() => frequency = Some(cursor.number()?),
            "last_selected_secs" if last_selected_secs.is_none() => {
                // A present field holding nothing, which is why the outer
                // `Option` becomes `Some(None)` rather than staying absent:
                // absent means the key was never written, and that is damage.
                let value = if cursor.null() {
                    None
                } else {
                    Some(cursor.number_u64()?)
                };
                last_selected_secs = Some(value);
            }
            _ => return None,
        }
        if !cursor.consume(',') {
            break;
        }
    }
    cursor.expect('}')?;

    Some(SelectionRecord {
        plugin: plugin?,
        item: item?,
        frequency: frequency?,
        last_selected_secs: last_selected_secs?,
    })
}

fn parse_query_affinities(cursor: &mut Cursor<'_>) -> Option<Vec<QueryAffinityRecord>> {
    cursor.expect('[')?;
    let mut records = Vec::new();
    if cursor.consume(']') {
        return Some(records);
    }
    loop {
        records.push(parse_query_affinity(cursor)?);
        if !cursor.consume(',') {
            break;
        }
    }
    cursor.expect(']')?;
    Some(records)
}

fn parse_query_affinity(cursor: &mut Cursor<'_>) -> Option<QueryAffinityRecord> {
    let mut plugin = None;
    let mut item = None;
    let mut query = None;
    let mut count = None;

    cursor.expect('{')?;
    loop {
        let key = cursor.string()?;
        cursor.expect(':')?;
        match key.as_str() {
            "plugin" if plugin.is_none() => plugin = Some(PluginId(cursor.string()?)),
            "item" if item.is_none() => item = Some(ItemId(cursor.string()?)),
            "query" if query.is_none() => query = Some(cursor.string()?),
            "count" if count.is_none() => count = Some(cursor.number()?),
            _ => return None,
        }
        if !cursor.consume(',') {
            break;
        }
    }
    cursor.expect('}')?;

    Some(QueryAffinityRecord {
        plugin: plugin?,
        item: item?,
        query: query?,
        count: count?,
    })
}
