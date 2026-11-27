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
//! not UTF-8, not a supported record — resolves to an empty history and is
//! repaired by the next [`SelectionHistoryStore::save`]. Saves report I/O
//! failures and explicitly reject invalid command records before touching disk;
//! neither failure prevents launcher startup.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crikey_core::{CommandRef, ItemId, PluginId};
use crikey_ranking::{
    CommandSelectionRecord, QueryAffinityRecord, SelectionHistorySnapshot, SelectionRecord,
    MAX_COMMAND_RECORDS,
};

use crate::startup_recovery::{read_bounded, write_json_string, Cursor};

/// Magic key and format version in one.
///
/// Version 1 contains item selections and query affinities only. Version 2 adds
/// exact successful command uses; migration never guesses commands from items.
const MAGIC_KEY: &str = "crikey_selection_history";

/// The version written; both version 1 and version 2 are accepted.
const FORMAT_VERSION: u32 = 2;

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

    /// Saves a selective reset before publishing the replacement in-memory history.
    /// A failed save leaves both the previous learned state and unrelated records intact.
    pub fn reset_item(
        &self,
        history: &mut crikey_ranking::SelectionHistory,
        owner: &PluginId,
        item: &ItemId,
    ) -> io::Result<bool> {
        let mut candidate = history.snapshot();
        let before = (candidate.selections.len(), candidate.query_affinities.len());
        candidate
            .selections
            .retain(|record| &record.plugin != owner || &record.item != item);
        candidate
            .query_affinities
            .retain(|record| &record.plugin != owner || &record.item != item);
        if before == (candidate.selections.len(), candidate.query_affinities.len()) {
            return Ok(false);
        }
        self.save(&candidate)?;
        *history = crikey_ranking::SelectionHistory::from_snapshot(candidate);
        Ok(true)
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
    /// Invalid command identities, duplicate references and zero-use records
    /// return `InvalidInput` before any I/O. Otherwise reports directory creation,
    /// write or rename failures; callers can continue without durable learning.
    ///
    /// [`StartupJournal::save`]: crate::StartupJournal::save
    pub fn save(&self, snapshot: &SelectionHistorySnapshot) -> io::Result<()> {
        let mut command_refs = std::collections::BTreeSet::new();
        for record in &snapshot.commands {
            record
                .reference
                .validate()
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
            if record.frequency == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "zero command frequency",
                ));
            }
            if !command_refs.insert(&record.reference) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "duplicate command identity",
                ));
            }
        }
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
// One JSON object with versioned keys, in a fixed order. The parser is strict for the
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
    let mut commands: Vec<&CommandSelectionRecord> = snapshot.commands.iter().collect();
    commands.sort_unstable_by(|left, right| {
        right
            .frequency
            .cmp(&left.frequency)
            .then_with(|| right.last_selected_secs.cmp(&left.last_selected_secs))
            .then_with(|| right.reference.cmp(&left.reference))
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

    let mut kept_commands = Vec::new();
    for record in commands.into_iter().take(MAX_COMMAND_RECORDS) {
        scratch.clear();
        push_command(&mut scratch, record);
        let cost = scratch.len() as u64 + u64::from(!kept_commands.is_empty());
        if used.saturating_add(cost) > budget {
            break;
        }
        used += cost;
        kept_commands.push(record.clone());
    }
    kept_commands.sort_unstable_by(|left, right| left.reference.cmp(&right.reference));

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
        commands: kept_commands,
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
    out.push_str("],\"commands\":[");
    for (index, record) in snapshot.commands.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_command(&mut out, record);
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

fn push_command(out: &mut String, record: &CommandSelectionRecord) {
    out.push_str("{\"provider_id\":");
    write_json_string(out, &record.reference.provider_id.0);
    out.push_str(",\"command_id\":");
    write_json_string(out, &record.reference.command_id);
    let _ = write!(
        out,
        ",\"frequency\":{},\"last_selected_secs\":{}}}",
        record.frequency, record.last_selected_secs
    );
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
    let version = cursor.number()?;
    if version != 1 && version != FORMAT_VERSION {
        return None;
    }

    let mut selections = None;
    let mut query_affinities = None;
    let mut commands = None;
    while cursor.consume(',') {
        let key = cursor.string()?;
        cursor.expect(':')?;
        match key.as_str() {
            "selections" if selections.is_none() => selections = Some(parse_selections(&mut cursor)?),
            "query_affinities" if query_affinities.is_none() => {
                query_affinities = Some(parse_query_affinities(&mut cursor)?)
            }
            "commands" if version == FORMAT_VERSION && commands.is_none() => {
                commands = Some(parse_commands(&mut cursor)?)
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
        commands: if version == 1 { Vec::new() } else { commands? },
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

fn parse_commands(cursor: &mut Cursor<'_>) -> Option<Vec<CommandSelectionRecord>> {
    cursor.expect('[')?;
    let mut records = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if cursor.consume(']') {
        return Some(records);
    }
    loop {
        if records.len() == MAX_COMMAND_RECORDS {
            return None;
        }
        let record = parse_command(cursor)?;
        if !seen.insert(record.reference.clone()) {
            return None;
        }
        records.push(record);
        if !cursor.consume(',') {
            break;
        }
    }
    cursor.expect(']')?;
    Some(records)
}

fn parse_command(cursor: &mut Cursor<'_>) -> Option<CommandSelectionRecord> {
    let mut provider_id = None;
    let mut command_id = None;
    let mut frequency = None;
    let mut last_selected_secs = None;
    cursor.expect('{')?;
    loop {
        let key = cursor.string()?;
        cursor.expect(':')?;
        match key.as_str() {
            "provider_id" if provider_id.is_none() => provider_id = Some(PluginId(cursor.string()?)),
            "command_id" if command_id.is_none() => command_id = Some(cursor.string()?),
            "frequency" if frequency.is_none() => frequency = Some(cursor.number()?),
            "last_selected_secs" if last_selected_secs.is_none() => {
                last_selected_secs = Some(cursor.number_u64()?);
            }
            _ => return None,
        }
        if !cursor.consume(',') {
            break;
        }
    }
    cursor.expect('}')?;
    let reference = CommandRef {
        provider_id: provider_id?,
        command_id: command_id?,
    };
    reference.validate().ok()?;
    let frequency = frequency?;
    if frequency == 0 {
        return None;
    }
    Some(CommandSelectionRecord {
        reference,
        frequency,
        last_selected_secs: last_selected_secs?,
    })
}

#[cfg(test)]
mod selective_reset_persistence_tests {
    use super::*;

    #[test]
    fn reset_saves_exact_owner_and_failed_save_keeps_memory() {
        let path = std::env::temp_dir().join(format!("crikey-selective-reset-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        let store = SelectionHistoryStore::new(path.join("ranking.json"));
        let item = ItemId("same-item".into());
        let owner = PluginId("owner-a".into());
        let other = PluginId("owner-b".into());
        let snapshot = SelectionHistorySnapshot {
            selections: vec![
                SelectionRecord {
                    plugin: owner.clone(),
                    item: item.clone(),
                    frequency: 2,
                    last_selected_secs: Some(1),
                },
                SelectionRecord {
                    plugin: other.clone(),
                    item: item.clone(),
                    frequency: 3,
                    last_selected_secs: Some(2),
                },
            ],
            query_affinities: vec![
                QueryAffinityRecord {
                    plugin: owner.clone(),
                    item: item.clone(),
                    query: "query".into(),
                    count: 2,
                },
                QueryAffinityRecord {
                    plugin: other.clone(),
                    item: item.clone(),
                    query: "query".into(),
                    count: 3,
                },
            ],
            commands: vec![
                CommandSelectionRecord {
                    reference: CommandRef {
                        provider_id: owner.clone(),
                        command_id: "first".into(),
                    },
                    frequency: 2,
                    last_selected_secs: 1,
                },
                CommandSelectionRecord {
                    reference: CommandRef {
                        provider_id: owner.clone(),
                        command_id: "second".into(),
                    },
                    frequency: 1,
                    last_selected_secs: 2,
                },
            ],
        };
        let mut history = crikey_ranking::SelectionHistory::from_snapshot(snapshot.clone());
        assert!(store.reset_item(&mut history, &owner, &item).unwrap());
        assert_eq!(store.load(), history.snapshot());
        assert_eq!(history.snapshot().selections[0].plugin, other);
        assert_eq!(history.snapshot().query_affinities.len(), 1);
        assert_eq!(history.snapshot().commands, snapshot.commands);
        assert!(!store.reset_item(&mut history, &owner, &item).unwrap());
        fs::remove_file(store.path()).unwrap();
        fs::create_dir(store.path()).unwrap();
        let mut history = crikey_ranking::SelectionHistory::from_snapshot(snapshot.clone());
        assert!(store.reset_item(&mut history, &owner, &item).is_err());
        assert_eq!(history.snapshot(), snapshot);
        fs::remove_dir_all(path).unwrap();
    }
}

#[cfg(test)]
mod command_budget_tests {
    use super::*;

    #[test]
    fn command_byte_budget_keeps_most_used_then_latest_with_exact_ties() {
        let record = |id: &str, frequency, last_selected_secs| CommandSelectionRecord {
            reference: CommandRef {
                provider_id: PluginId("p".into()),
                command_id: id.into(),
            },
            frequency,
            last_selected_secs,
        };
        let mut snapshot = SelectionHistorySnapshot {
            commands: vec![
                record("a", 2, 1),
                record("b", 1, 2),
                record("c", 1, 2),
                record("d", 1, 1),
            ],
            ..SelectionHistorySnapshot::default()
        };
        let mut scratch = String::new();
        push_command(&mut scratch, &snapshot.commands[0]);
        let budget = (envelope_bytes() + scratch.len() * 2 + 1) as u64;
        let kept = within_budget(&snapshot, budget);
        assert_eq!(
            kept.commands
                .iter()
                .map(|record| record.reference.command_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert!(serialize(&kept).len() as u64 <= budget);
        snapshot.commands.reverse();
        assert_eq!(within_budget(&snapshot, budget), kept);
    }
}
