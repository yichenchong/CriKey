//! Submitted nonsecret root searches only. Session recall is the default; persistence is explicit opt-in.
use crate::startup_recovery::{read_bounded, write_json_string, Cursor};
use std::{
    collections::VecDeque,
    fs, io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

// Same unique-sibling/rename discipline as selection history, with private staging.
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut staging = path.as_os_str().to_owned();
    staging.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let staging = PathBuf::from(staging);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&staging)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&staging, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(staging);
    }
    result
}

const MAX_ENTRIES: usize = 100;
const MAX_QUERY_BYTES: usize = 4096;
const MAX_TOTAL_BYTES: usize = 256 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub trait QueryHistoryClock: Send + Sync {
    fn now(&self) -> Duration;
}
#[derive(Debug)]
pub struct MonotonicQueryHistoryClock {
    origin: Instant,
}
impl Default for MonotonicQueryHistoryClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}
impl QueryHistoryClock for MonotonicQueryHistoryClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryHistoryMode {
    Off,
    Session,
    Persistent,
}
#[derive(Clone)]
struct Entry {
    text: String,
    submitted: Duration,
}
/// Diagnostics omit query text, storage paths and clock state.
pub struct QueryHistoryStore {
    entries: VecDeque<Entry>,
    mode: QueryHistoryMode,
    path: Option<PathBuf>,
    clock: Arc<dyn QueryHistoryClock>,
    expiry: Duration,
}
impl std::fmt::Debug for QueryHistoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryHistoryStore")
            .field("mode", &self.mode)
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}
impl QueryHistoryStore {
    pub fn new(path: impl Into<PathBuf>, expiry: Duration) -> Self {
        Self::with_clock(path, expiry, Arc::new(MonotonicQueryHistoryClock::default()))
    }
    pub fn with_clock(path: impl Into<PathBuf>, expiry: Duration, clock: Arc<dyn QueryHistoryClock>) -> Self {
        Self {
            entries: VecDeque::new(),
            mode: QueryHistoryMode::Session,
            path: Some(path.into()),
            clock,
            expiry,
        }
    }
    /// Session recall works even when no safe per-user state directory exists.
    pub fn session(expiry: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            mode: QueryHistoryMode::Session,
            path: None,
            clock: Arc::new(MonotonicQueryHistoryClock::default()),
            expiry,
        }
    }
    pub fn mode(&self) -> QueryHistoryMode {
        self.mode
    }
    /// No file is read until the caller supplies an explicit Persistent opt-in.
    /// Disabling persistence stops writes immediately; deletion failure is reported.
    pub fn set_mode(&mut self, mode: QueryHistoryMode) -> io::Result<()> {
        if mode == self.mode && mode != QueryHistoryMode::Session {
            return Ok(());
        }
        let mut entries = self.live_entries();
        if mode == QueryHistoryMode::Persistent {
            let path = self.path.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "no private state directory is available for durable query history",
                )
            })?;
            if entries.is_empty() {
                if let Some(text) = read_bounded(path, MAX_FILE_BYTES) {
                    let queries = parse(&text)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid query history"))?;
                    let now = self.clock.now();
                    entries = queries
                        .into_iter()
                        .map(|text| Entry { text, submitted: now })
                        .collect();
                } else if path.exists() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unreadable query history",
                    ));
                }
            }
            self.save_entries(&entries)?;
        } else {
            // Off/session stops collection/persistence immediately, even if
            // deleting an old durable file fails and must be reported.
            self.mode = mode;
            self.entries = if mode == QueryHistoryMode::Off {
                VecDeque::new()
            } else {
                entries
            };
            return self.remove_saved();
        }
        self.entries = entries;
        self.mode = mode;
        Ok(())
    }
    /// Call only on deliberate nonsecret root submission, never on keystrokes,
    /// action-panel searches, structured argument values or password fields.
    pub fn record_submitted(&mut self, query: &str) -> io::Result<bool> {
        if self.mode == QueryHistoryMode::Off || query.trim().is_empty() {
            return Ok(false);
        }
        if query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "query exceeds history bounds",
            ));
        }
        let mut entries = self.live_entries();
        entries.retain(|entry| entry.text != query);
        entries.push_back(Entry {
            text: query.into(),
            submitted: self.clock.now(),
        });
        let mut bytes: usize = entries.iter().map(|entry| entry.text.len()).sum();
        while entries.len() > MAX_ENTRIES || bytes > MAX_TOTAL_BYTES {
            if let Some(entry) = entries.pop_front() {
                bytes -= entry.text.len();
            }
        }
        if self.mode == QueryHistoryMode::Persistent {
            self.save_entries(&entries)?;
        }
        self.entries = entries;
        Ok(true)
    }
    /// Newest first; expiry uses the injected monotonic clock, never sleeps/wall time.
    pub fn snapshot(&self) -> Vec<String> {
        self.live_entries()
            .into_iter()
            .rev()
            .map(|entry| entry.text)
            .collect()
    }
    pub fn clear(&mut self) -> io::Result<()> {
        self.entries.clear();
        self.remove_saved()
    }
    fn remove_saved(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
    fn live_entries(&self) -> VecDeque<Entry> {
        let now = self.clock.now();
        self.entries
            .iter()
            .filter(|entry| {
                now.checked_sub(entry.submitted)
                    .is_some_and(|age| age < self.expiry)
            })
            .cloned()
            .collect()
    }
    fn save_entries(&self, entries: &VecDeque<Entry>) -> io::Result<()> {
        let mut text = String::from("{\"crikey_query_history\":1,\"queries\":[");
        for (index, entry) in entries.iter().enumerate() {
            if index != 0 {
                text.push(',');
            }
            write_json_string(&mut text, &entry.text);
        }
        text.push_str("]}");
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "query history exceeds file bound",
            ));
        }
        let path = self.path.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "no private state directory is available for durable query history",
            )
        })?;
        atomic_write(path, text.as_bytes())
    }
}
fn parse(text: &str) -> Option<Vec<String>> {
    let mut cursor = Cursor::new(text);
    cursor.expect('{')?;
    if cursor.string()? != "crikey_query_history" {
        return None;
    }
    cursor.expect(':')?;
    if cursor.number()? != 1 {
        return None;
    }
    cursor.expect(',')?;
    if cursor.string()? != "queries" {
        return None;
    }
    cursor.expect(':')?;
    cursor.expect('[')?;
    let mut entries = Vec::new();
    let mut bytes = 0;
    if !cursor.consume(']') {
        loop {
            if entries.len() == MAX_ENTRIES {
                return None;
            }
            let query = cursor.string()?;
            if query.trim().is_empty()
                || query.len() > MAX_QUERY_BYTES
                || query.chars().any(char::is_control)
                || entries.contains(&query)
            {
                return None;
            }
            bytes += query.len();
            if bytes > MAX_TOTAL_BYTES {
                return None;
            }
            entries.push(query);
            if cursor.consume(']') {
                break;
            }
            cursor.expect(',')?;
        }
    }
    cursor.expect('}')?;
    cursor.skip_whitespace();
    cursor.rest().is_empty().then_some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Clock(AtomicU64);
    impl QueryHistoryClock for Clock {
        fn now(&self) -> Duration {
            Duration::from_secs(self.0.load(Ordering::Relaxed))
        }
    }
    #[test]
    fn session_default_is_private_bounded_and_expires_with_fake_clock() {
        let clock = Arc::new(Clock(AtomicU64::new(0)));
        let path = std::env::temp_dir().join(format!("crikey-recall-session-{}", std::process::id()));
        let mut store = QueryHistoryStore::with_clock(&path, Duration::from_secs(60), clock.clone());
        for index in 0..120 {
            store.record_submitted(&format!("query {index}")).unwrap();
        }
        assert_eq!(store.snapshot().len(), 100);
        assert_eq!(store.snapshot()[0], "query 119");
        assert!(!path.exists(), "session recall must never write");
        clock.0.store(60, Ordering::Relaxed);
        assert!(store.snapshot().is_empty());
        store.set_mode(QueryHistoryMode::Off).unwrap();
        assert!(!store.record_submitted("ignored").unwrap());
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    #[test]
    fn durable_is_opt_in_and_clear_off_remove_saved_queries() {
        let path =
            std::env::temp_dir().join(format!("crikey-query-history-persistent-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut first = QueryHistoryStore::new(&path, Duration::from_secs(600));
        first.record_submitted("first").unwrap();
        assert!(!path.exists());
        first.set_mode(QueryHistoryMode::Persistent).unwrap();
        assert!(path.exists());
        first.record_submitted("second").unwrap();
        let mut restarted = QueryHistoryStore::new(&path, Duration::from_secs(600));
        assert!(
            restarted.snapshot().is_empty(),
            "session default never reads durable text"
        );
        restarted.set_mode(QueryHistoryMode::Persistent).unwrap();
        assert_eq!(restarted.snapshot(), ["second", "first"]);
        restarted.clear().unwrap();
        assert!(restarted.snapshot().is_empty());
        assert!(!path.exists());
        restarted.record_submitted("third").unwrap();
        restarted.set_mode(QueryHistoryMode::Session).unwrap();
        assert!(!path.exists());
        assert_eq!(restarted.snapshot(), ["third"]);
        restarted.set_mode(QueryHistoryMode::Off).unwrap();
        assert!(restarted.snapshot().is_empty());
        assert!(!restarted.record_submitted("ignored").unwrap());
    }
}

#[cfg(test)]
mod unavailable_storage_tests {
    use super::*;
    #[test]
    fn no_private_directory_still_has_session_recall_but_refuses_durable_opt_in() {
        let mut history = QueryHistoryStore::session(Duration::from_secs(60));
        history.record_submitted("session").unwrap();
        assert!(history.set_mode(QueryHistoryMode::Persistent).is_err());
        assert_eq!(history.mode(), QueryHistoryMode::Session);
        assert_eq!(history.snapshot(), ["session"]);
        history.clear().unwrap();
        assert!(history.snapshot().is_empty());
    }

    #[test]
    fn failed_saved_file_removal_never_keeps_collection_enabled_after_off() {
        let directory = std::env::temp_dir().join(format!(
            "crikey-query-history-delete-refusal-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let mut history = QueryHistoryStore::new(&directory, Duration::from_secs(60));
        history.record_submitted("before off").unwrap();
        assert!(history.set_mode(QueryHistoryMode::Off).is_err());
        assert_eq!(history.mode(), QueryHistoryMode::Off);
        assert!(history.snapshot().is_empty());
        assert!(!history.record_submitted("must not collect").unwrap());
        fs::remove_dir(directory).unwrap();
    }
}
