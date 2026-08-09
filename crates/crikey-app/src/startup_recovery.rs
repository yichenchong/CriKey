//! Startup recovery and safe mode (spec 24.2).
//!
//! Two requirements share one on-disk record:
//!
//! - CriKey shall record which plugins were active during an abnormal
//!   shutdown.
//! - On repeated startup failure, CriKey shall enter safe mode with
//!   third-party plugins disabled.
//!
//! A "failure" is a launch that called [`StartupJournal::begin_startup`] and
//! never reached [`StartupJournal::mark_ready`]. That is deliberately the only
//! definition available to a process that dies mid-boot: nothing running after
//! the crash can be trusted to have observed it, so the *next* launch reads the
//! attempt the previous one left behind and draws the conclusion.
//!
//! [`StartupJournal::begin_startup`] therefore reports the mode implied by the
//! failures already persisted, and only then records the attempt it was called
//! for. Counting the current attempt first would trip safe mode one launch
//! early and disable every third-party plugin on a merely unlucky install.
//!
//! The journal is the mechanism that is supposed to survive crashes, so it must
//! never become a crash source itself: a missing file is a first launch and a
//! damaged file is indistinguishable from one. Both load as fresh, admit the
//! startup normally, and are repaired by the next [`StartupJournal::save`].
//!
//! Deciding the mode buys nothing on its own. [`admitted_plugin_roots`] is the
//! seam where the decision meets the loaders: under safe mode no third-party
//! root is offered to any provider, so the packages are not merely disabled
//! after loading — they are never discovered.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crikey_core::PluginId;

/// Consecutive failed startups that put the next launch into safe mode.
pub const SAFE_MODE_AFTER_FAILURES: u32 = 3;

/// How a launch was admitted (spec 24.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupMode {
    /// Every configured plugin root is offered to the providers.
    Normal,
    /// Third-party plugins are disabled after repeated startup failure. The
    /// count is the real number of consecutive failures observed on disk, not
    /// the threshold, so a diagnostic can report how deep the loop went.
    SafeMode { consecutive_failures: u32 },
}

/// The persistent startup record, remembering the path it was loaded from.
///
/// Mutations are in-memory until [`StartupJournal::save`], which is what lets a
/// launch decide its mode, run, and commit a single consistent record — rather
/// than leaving a half-updated file behind if it dies between two writes.
#[derive(Debug)]
pub struct StartupJournal {
    path: PathBuf,
    /// Startups since the last one that reached ready, including the attempt
    /// in progress once `admitted` is set.
    consecutive_failures: u32,
    /// Plugins active in the launch that owns this record. Read by the *next*
    /// launch, where a non-empty set means the previous shutdown was abnormal.
    active: Vec<PluginId>,
    /// The mode this process was admitted under, once decided. Present so a
    /// composition root may re-declare its active plugin set after the
    /// providers have loaded — the ids are not knowable before discovery runs,
    /// and the crash record is worthless without them — without charging the
    /// install a second failed attempt or changing its verdict mid-boot.
    admitted: Option<StartupMode>,
}

impl StartupJournal {
    /// The largest journal accepted from disk.
    ///
    /// A real record is one integer and one id per loaded plugin: a few
    /// hundred third-party plugins still fit in a few kilobytes, so anything
    /// past this ceiling was not written by CriKey. Reading it in full to find
    /// that out would let a hostile or accidentally huge file decide how much
    /// this process allocates before it has a window, and an allocator abort
    /// is not something `.ok()` can recover from — the one failure mode this
    /// layer exists to rule out. Over-limit is therefore corruption, handled
    /// exactly as invalid bytes are.
    pub const MAX_BYTES: u64 = 64 * 1024;

    /// Reads the journal at `path`, treating an absent, oversized or
    /// unreadable record as a first launch.
    ///
    /// Never fails: a boot-time recovery mechanism that can refuse to load is a
    /// boot failure no recovery path can catch.
    pub fn load(path: &Path) -> Self {
        let recovered = read_bounded(path, Self::MAX_BYTES).and_then(|text| parse(&text));
        let (consecutive_failures, active) = recovered.unwrap_or((0, Vec::new()));
        Self {
            path: path.to_path_buf(),
            consecutive_failures,
            active,
            admitted: None,
        }
    }

    /// Admits this launch, reporting the mode implied by the failures already
    /// recorded, and records this attempt as unfinished.
    ///
    /// `plugins` is the set to blame if this launch never reaches ready. A
    /// later call on the same journal refreshes that set and repeats the same
    /// verdict; only the first call counts an attempt.
    pub fn begin_startup(&mut self, plugins: &[PluginId]) -> StartupMode {
        let mode = match &self.admitted {
            Some(mode) => mode.clone(),
            None => {
                let mode = if self.consecutive_failures >= SAFE_MODE_AFTER_FAILURES {
                    StartupMode::SafeMode {
                        consecutive_failures: self.consecutive_failures,
                    }
                } else {
                    StartupMode::Normal
                };
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.admitted = Some(mode.clone());
                mode
            }
        };

        self.record_active_plugins(plugins);
        mode
    }

    /// Refreshes the plugin set this launch would blame if it died now,
    /// without touching its verdict or charging it an attempt.
    ///
    /// The composition root calls this as each provider finishes loading: the
    /// ids do not exist until discovery has run, and a launch that dies
    /// between two providers must still leave behind the plugins that were
    /// actually active at that moment (spec 24.2). Recording them only once
    /// every provider is up records nothing for exactly the crashes this file
    /// exists to explain.
    pub fn record_active_plugins(&mut self, plugins: &[PluginId]) {
        self.active.clear();
        self.active.extend_from_slice(plugins);
    }

    /// Records that this launch reached a usable state, clearing the failure
    /// run and closing its admission. A later startup in the same process must
    /// decide its mode from the reset count rather than replaying this launch's
    /// old verdict.
    ///
    /// A reset rather than a decrement: safe mode is about *consecutive*
    /// failures, so one successful boot must take a looping install straight
    /// back out of it.
    pub fn mark_ready(&mut self) {
        self.consecutive_failures = 0;
        self.admitted = None;
    }

    /// Records that this launch shut down deliberately, so no plugin is blamed
    /// for a crash that did not happen.
    pub fn mark_clean_shutdown(&mut self) {
        self.active.clear();
    }

    /// The plugins the previous launch had active when it died, or the plugins
    /// this launch would blame if it died now.
    pub fn active_during_abnormal_shutdown(&self) -> &[PluginId] {
        &self.active
    }

    /// Commits the record to the path it was loaded from.
    ///
    /// Written to a sibling temporary file and renamed, so a crash during the
    /// save cannot replace a readable journal with a truncated one. The
    /// staging name is unique to this save: a fixed `<journal>.tmp` is shared
    /// by every process and thread that ever saves, and two concurrent saves
    /// through one inode can publish a mixture of both records — losing the
    /// crash-loop count that safe mode is decided from. Nothing here takes a
    /// lock, so uniqueness is the whole guarantee: each save writes a file
    /// only it names, and the rename that publishes it is atomic.
    pub fn save(&self) -> io::Result<()> {
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

        fs::write(&staging, self.serialize())?;
        match fs::rename(&staging, &self.path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&staging);
                Err(error)
            }
        }
    }

    fn serialize(&self) -> String {
        let mut out = String::from("{\"consecutive_failures\":");
        let _ = write!(out, "{}", self.consecutive_failures);
        out.push_str(",\"active\":[");
        for (index, plugin) in self.active.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            write_json_string(&mut out, &plugin.0);
        }
        out.push_str("]}");
        out
    }
}

/// The third-party plugin roots a launch in `mode` may offer to a provider.
///
/// Safe mode admits none: spec 24.2 disables third-party plugins, and the only
/// way to prove a package was not loaded is for the loader never to see it.
/// First-party built-ins are not roots, so they are untouched.
pub fn admitted_plugin_roots(mode: &StartupMode, roots: &[PathBuf]) -> Vec<PathBuf> {
    match mode {
        StartupMode::Normal => roots.to_vec(),
        StartupMode::SafeMode { .. } => Vec::new(),
    }
}

/// The plugins an operator has switched off (spec 21.2; `crikey plugin disable`).
///
/// The companion of [`admitted_plugin_roots`], and the same reasoning: a
/// decision about what may load is worth nothing until it reaches the loader,
/// and the only way to prove a plugin was not loaded is for no worker to be
/// spawned and no registration to happen. Safe mode withholds whole roots
/// because it distrusts the launch; this withholds named plugins because the
/// operator asked, so it has to be per-plugin and cannot be a root list.
///
/// Membership is keyed on the *namespaced* plugin id the pipeline uses —
/// `legacy.foo`, `modern.foo`, `native.foo` (spec 10.2). Two runtimes may ship
/// the same bare id, so the bare id is not a key that identifies one plugin;
/// resolving what an operator typed to a namespaced id belongs to the command
/// line, which can see the whole inventory and refuse an ambiguous name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisabledPlugins(BTreeSet<String>);

impl DisabledPlugins {
    /// Collects namespaced plugin ids into a disabled set.
    pub fn from_ids<I>(ids: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        Self(ids.into_iter().collect())
    }

    /// Whether `plugin` must not be loaded.
    pub fn blocks(&self, plugin: &PluginId) -> bool {
        self.0.contains(&plugin.0)
    }
}

/// The reason a provider records for a plugin held back by [`DisabledPlugins`].
///
/// One spelling, shared by the three providers: an operator grepping the
/// startup diagnostics for why a plugin is missing must find the same sentence
/// whichever runtime it belongs to.
pub const DISABLED_BY_CONFIGURATION: &str = "disabled by configuration (crikey plugin enable to restore)";

/// Distinguishes one save's staging file from every other save's in this
/// process; the pid distinguishes it from every other process's.
static SAVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Reads at most `max_bytes` from `path`, or `None` if the file is absent,
/// non-regular, unreadable, not UTF-8, or larger than that.
///
/// The ceiling is applied to the *reader*, not to a stat of the path: a size
/// read before the open is a guess about a file another process may still be
/// growing. Unix opens use no-follow and non-blocking flags before checking the
/// handle type, so a path swapped to a symlink or named pipe cannot hang boot.
/// Taking the reader bounds the allocation whatever a regular file's contents
/// turn out to be.
///
/// Shared with the selection history store: both are per-user state files read
/// during startup, and both must be incapable of failing one.
pub(crate) fn read_bounded(path: &Path, max_bytes: u64) -> Option<String> {
    let file = open_regular_file(path)?;
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }
    let mut text = String::new();
    let read = file
        .take(max_bytes.saturating_add(1))
        .read_to_string(&mut text)
        .ok()?;
    u64::try_from(read)
        .is_ok_and(|read| read <= max_bytes)
        .then_some(text)
}

fn open_regular_file(path: &Path) -> Option<fs::File> {
    #[cfg(unix)]
    {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()
    }
    #[cfg(not(unix))]
    {
        let metadata = fs::symlink_metadata(path).ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }
        fs::File::open(path).ok()
    }
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------
//
// The record is a two-field JSON object. `crikey-app` has no serde dependency
// and one is not worth adding for this, so the writer and a parser for exactly
// this shape live here. The parser is strict on purpose: anything it does not
// recognize is corruption, and corruption loads as a fresh journal.
//
// [`write_json_string`] and [`Cursor`] are crate-visible because the selection
// history store persists the same way for the same reason, and a second
// hand-rolled JSON reader in the same crate would be a second set of escaping
// and truncation bugs to find.

pub(crate) fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control < '\u{20}' => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Parses a saved record, or `None` if the bytes are not one.
fn parse(text: &str) -> Option<(u32, Vec<PluginId>)> {
    let mut cursor = Cursor::new(text);
    let mut failures = None;
    let mut active = None;

    cursor.expect('{')?;
    if !cursor.consume('}') {
        loop {
            let key = cursor.string()?;
            cursor.expect(':')?;
            match key.as_str() {
                "consecutive_failures" if failures.is_none() => failures = Some(cursor.number()?),
                "active" if active.is_none() => active = Some(cursor.string_array()?),
                // An unknown or repeated key means these bytes were not
                // written by this version, and guessing at them would be
                // inventing recovery state.
                _ => return None,
            }
            if !cursor.consume(',') {
                break;
            }
        }
        cursor.expect('}')?;
    }
    cursor.skip_whitespace();
    if cursor.rest().is_empty() {
        Some((failures?, active?))
    } else {
        None
    }
}

pub(crate) struct Cursor<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    pub(crate) fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    pub(crate) fn skip_whitespace(&mut self) {
        let trimmed = self.rest().trim_start_matches([' ', '\t', '\n', '\r']);
        self.at = self.text.len() - trimmed.len();
    }

    /// Consumes `expected` if it is the next non-space character.
    pub(crate) fn consume(&mut self, expected: char) -> bool {
        self.skip_whitespace();
        match self.rest().strip_prefix(expected) {
            Some(remainder) => {
                self.at = self.text.len() - remainder.len();
                true
            }
            None => false,
        }
    }

    pub(crate) fn expect(&mut self, expected: char) -> Option<()> {
        self.consume(expected).then_some(())
    }

    /// Consumes the literal `null`, which is how an absent optional field is
    /// written. Omitting the key instead would leave a strict parser unable to
    /// tell a legitimately empty field from a truncated record.
    pub(crate) fn null(&mut self) -> bool {
        self.skip_whitespace();
        match self.rest().strip_prefix("null") {
            Some(remainder) => {
                self.at = self.text.len() - remainder.len();
                true
            }
            None => false,
        }
    }

    pub(crate) fn number(&mut self) -> Option<u32> {
        u32::try_from(self.number_u64()?).ok()
    }

    /// Reads an unsigned decimal integer, rejecting one too large to represent
    /// rather than saturating: a saturated count is a fabricated one.
    pub(crate) fn number_u64(&mut self) -> Option<u64> {
        self.skip_whitespace();
        let digits = self.rest();
        let end = digits
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(digits.len());
        if end == 0 {
            return None;
        }
        self.at += end;
        digits[..end].parse().ok()
    }

    pub(crate) fn string(&mut self) -> Option<String> {
        self.expect('"')?;
        let mut value = String::new();
        let mut characters = self.rest().char_indices();
        loop {
            let (offset, character) = characters.next()?;
            match character {
                '"' => {
                    self.at += offset + character.len_utf8();
                    return Some(value);
                }
                '\\' => {
                    let (_, escape) = characters.next()?;
                    match escape {
                        '"' => value.push('"'),
                        '\\' => value.push('\\'),
                        '/' => value.push('/'),
                        'b' => value.push('\u{8}'),
                        'f' => value.push('\u{c}'),
                        'n' => value.push('\n'),
                        'r' => value.push('\r'),
                        't' => value.push('\t'),
                        'u' => {
                            let mut code = 0u32;
                            for _ in 0..4 {
                                let (_, digit) = characters.next()?;
                                code = code * 16 + digit.to_digit(16)?;
                            }
                            // Surrogate halves are never written here, so a
                            // lone one is damage rather than a pair to join.
                            value.push(char::from_u32(code)?);
                        }
                        _ => return None,
                    }
                }
                // Raw control characters are illegal inside a JSON string, and
                // are exactly what a truncated write leaves behind.
                control if control < '\u{20}' => return None,
                other => value.push(other),
            }
        }
    }

    fn string_array(&mut self) -> Option<Vec<PluginId>> {
        self.expect('[')?;
        let mut values = Vec::new();
        if self.consume(']') {
            return Some(values);
        }
        loop {
            values.push(PluginId(self.string()?));
            if !self.consume(',') {
                break;
            }
        }
        self.expect(']')?;
        Some(values)
    }
}
