//! The compatibility matrix and the real-plugin corpus as *typed, tested data*
//! (spec 14.10, 14.12, 27.4).
//!
//! Spec 14.10 requires the matrix to be version-controlled **and tested**. A
//! markdown table satisfies the first half only: nothing stops it from claiming
//! `full` for a symbol the shim never defines. So the published artefacts are
//! two TOML files — `compatibility/api-matrix/matrix.toml` and
//! `compatibility/real-plugin-corpus/corpus.toml` — and this module is the
//! mechanism that turns them into values the test suite can falsify.
//!
//! Three rules shape everything below.
//!
//! * **Validation happens at load time.** An unknown status spelling, an empty
//!   locator, a duplicate key, an unexplained caveat and an unpinned revision
//!   are all [`MatrixError`]s from `parse`, each naming the offending row. A
//!   file that loads is therefore already known to be well-formed, so no
//!   consumer needs a "did you check?" contract.
//! * **No spelling is ever defaulted.** `ApiSupport::parse_slug` and
//!   [`PluginClassification::parse_slug`] are total and case-sensitive; an
//!   unrecognised value is a typed error carrying the value, never a silent
//!   fallback to `planned` or `untested`, which would understate a gap.
//! * **`symbol = "*"` is ordinary text.** Storage and [`CompatibilityMatrix::get`]
//!   treat it literally. Exactly one place gives it meaning:
//!   [`CompatibilityMatrix::classify`] tries the exact `(module, symbol)` pair
//!   first and only then falls back to `(module, "*")`. That lets
//!   `keypirinha_wintypes` carry one catch-all row instead of an enumeration of
//!   every Win32 name, without turning the matrix into a pattern-matching
//!   engine — a glob syntax was rejected because "which pattern won?" is not a
//!   question a published compatibility claim should raise.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ApiSupport;

/// One classified legacy API (spec 14.10).
///
/// `symbol` is either a documented name (`"Plugin.on_start"`) or the literal
/// `"*"` catch-all for the rest of its module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixEntry {
    pub module: String,
    pub symbol: String,
    pub status: ApiSupport,
    /// Why the API differs. Mandatory for `behavioural-difference` and
    /// `partial`, empty otherwise.
    pub notes: String,
}

/// One referenced upstream Keypirinha package (spec 27.4).
///
/// Packages are referenced, never vendored: `source` plus a pinned `revision`
/// is what makes a classification reproducible against the exact tree it
/// describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusEntry {
    pub id: String,
    pub source: String,
    /// A full 40-character commit hash. Tags and branch names move and cannot
    /// reproduce a classification.
    pub revision: String,
    pub licence: String,
    pub classification: PluginClassification,
    pub notes: String,
}

/// Observed compatibility of one real published package (spec 27.4).
///
/// Declaration order is the report order and is part of the wire contract of
/// `crikey dev compatibility-report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginClassification {
    WorksUnchanged,
    WorksWithConfigurationChanges,
    WorksWithMinimalSourceChanges,
    WindowsOnlyButCompatible,
    BlockedMissingApis,
    BlockedPythonVersion,
    BlockedUndocumentedBehaviour,
    WorksOnlyUnderLegacyOptimized,
    RequiresLegacyStrict,
    /// Referenced but not yet exercised. An honest state; it may never be
    /// omitted from the published totals, which would overstate coverage.
    Untested,
}

impl PluginClassification {
    /// Declaration order, used to render deterministic reports.
    pub const ALL: [PluginClassification; 10] = [
        Self::WorksUnchanged,
        Self::WorksWithConfigurationChanges,
        Self::WorksWithMinimalSourceChanges,
        Self::WindowsOnlyButCompatible,
        Self::BlockedMissingApis,
        Self::BlockedPythonVersion,
        Self::BlockedUndocumentedBehaviour,
        Self::WorksOnlyUnderLegacyOptimized,
        Self::RequiresLegacyStrict,
        Self::Untested,
    ];

    /// The kebab-case spelling used in `corpus.toml`.
    pub fn slug(self) -> &'static str {
        match self {
            Self::WorksUnchanged => "works-unchanged",
            Self::WorksWithConfigurationChanges => "works-with-configuration-changes",
            Self::WorksWithMinimalSourceChanges => "works-with-minimal-source-changes",
            Self::WindowsOnlyButCompatible => "windows-only-but-compatible",
            Self::BlockedMissingApis => "blocked-missing-apis",
            Self::BlockedPythonVersion => "blocked-python-version",
            Self::BlockedUndocumentedBehaviour => "blocked-undocumented-behaviour",
            Self::WorksOnlyUnderLegacyOptimized => "works-only-under-legacy-optimized",
            Self::RequiresLegacyStrict => "requires-legacy-strict",
            Self::Untested => "untested",
        }
    }

    /// Whether a package with this classification may be presented as
    /// cross-platform (acceptance 31.31).
    ///
    /// True only for the classifications that *assert* the package runs off
    /// Windows. Three groups are deliberately excluded, and each exclusion is
    /// load-bearing:
    ///
    /// * `windows-only-but-compatible` is the state acceptance 31.31 names: the
    ///   package works, on Windows, and saying otherwise is the exact
    ///   misrepresentation the criterion forbids.
    /// * The `blocked-*` states describe a package that runs nowhere. "Not
    ///   portable" is the weaker of the two true statements about it, and a
    ///   portable verdict would be false on every platform including Windows.
    /// * `untested` is an absence of evidence. A package that has never been
    ///   exercised has demonstrated nothing, so reading it as portable would
    ///   let coverage gaps advertise themselves as cross-platform support —
    ///   portability is a claim that must be earned, never defaulted into.
    pub fn is_portable(self) -> bool {
        match self {
            Self::WorksUnchanged
            | Self::WorksWithConfigurationChanges
            | Self::WorksWithMinimalSourceChanges
            | Self::WorksOnlyUnderLegacyOptimized
            | Self::RequiresLegacyStrict => true,
            Self::WindowsOnlyButCompatible
            | Self::BlockedMissingApis
            | Self::BlockedPythonVersion
            | Self::BlockedUndocumentedBehaviour
            | Self::Untested => false,
        }
    }

    /// Total and case-sensitive: an unknown spelling is `None`, never a
    /// silently defaulted classification.
    pub fn parse_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.slug() == slug)
    }
}

impl std::fmt::Display for PluginClassification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// Every way the committed compatibility data can be wrong.
///
/// One variant per rejection, each carrying the locator a maintainer needs:
/// a `String` message would make "which row?" unanswerable in a test.
#[derive(Debug, thiserror::Error)]
pub enum MatrixError {
    #[error("cannot read compatibility data from {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed compatibility data: {source}")]
    Syntax {
        #[source]
        source: toml::de::Error,
    },

    #[error("api[{index}] has an empty `{field}`, so it identifies no API")]
    EmptyApiField { index: usize, field: &'static str },
    #[error("compatibility matrix version {version} is unsupported; expected {expected}")]
    UnsupportedMatrixVersion { version: u32, expected: u32 },
    #[error("api[{index}] uses unknown module `{module}`; expected one of the documented legacy modules")]
    UnknownApiModule { index: usize, module: String },
    #[error("package corpus version {version} is unsupported; expected {expected}")]
    UnsupportedCorpusVersion { version: u32, expected: u32 },

    #[error("package[{index}] `{id}` has an empty `{field}`")]
    EmptyPackageField {
        index: usize,
        id: String,
        field: &'static str,
    },

    #[error("package `{id}` declares source `{url}`, but corpus sources must be valid https URLs")]
    InvalidPackageSource { id: String, url: String },

    #[error(
        "{module}::{symbol} declares unknown status `{value}`; expected one of \
         full, behavioural-difference, windows-only, partial, unsupported, planned"
    )]
    UnknownApiStatus {
        module: String,
        symbol: String,
        value: String,
    },

    #[error("package `{package}` declares unknown classification `{value}`")]
    UnknownClassification { package: String, value: String },

    #[error("{module}::{symbol} is classified twice, so its support level is ambiguous")]
    DuplicateApi { module: String, symbol: String },

    #[error("package `{id}` is listed twice, so its classification is ambiguous")]
    DuplicatePackage { id: String },

    #[error(
        "{module}::{symbol} is classified `{status}` but carries no notes; an \
         unexplained caveat tells a plugin author nothing (spec 14.10, 14.12)"
    )]
    MissingNotes {
        module: String,
        symbol: String,
        status: ApiSupport,
    },

    #[error(
        "package `{id}` pins revision `{revision}`, which is not a 40-character \
         commit hash; tags and branches move and cannot reproduce a classification"
    )]
    UnpinnedRevision { id: String, revision: String },
}

/// Schema versions understood by this crate. A file with a newer version must
/// fail closed: parsing it as the old shape could silently misclassify APIs.
const MATRIX_SCHEMA_VERSION: u32 = 1;
const CORPUS_SCHEMA_VERSION: u32 = 1;
const DOCUMENTED_MODULES: [&str; 4] = [
    "keypirinha",
    "keypirinha_util",
    "keypirinha_net",
    "keypirinha_wintypes",
];
/// A full commit hash, in characters. Anything shorter is ambiguous and
/// anything else is not a commit at all.
const REVISION_LENGTH: usize = 40;

/// Intermediate for one `[[api]]` table.
///
/// Every field is `#[serde(default)]` so a missing key surfaces as the located
/// [`MatrixError::EmptyApiField`] rather than an anonymous serde error that
/// cannot say which row was wrong.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApi {
    #[serde(default)]
    module: String,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    notes: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMatrix {
    #[serde(rename = "matrix-version")]
    matrix_version: u32,
    #[serde(default)]
    api: Vec<RawApi>,
}

/// Intermediate for one `[[package]]` table. Same `default` reasoning as
/// [`RawApi`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPackage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    revision: String,
    #[serde(default)]
    licence: String,
    #[serde(default)]
    classification: String,
    #[serde(default)]
    notes: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCorpus {
    #[serde(rename = "corpus-version")]
    corpus_version: u32,
    #[serde(default)]
    package: Vec<RawPackage>,
}

fn read_to_string(path: &Path) -> Result<String, MatrixError> {
    std::fs::read_to_string(path).map_err(|source| MatrixError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// The version-controlled classification of the documented legacy API surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityMatrix {
    version: u32,
    entries: Vec<MatrixEntry>,
}

impl CompatibilityMatrix {
    /// Reads and validates `matrix.toml`.
    pub fn load(path: &Path) -> Result<Self, MatrixError> {
        Self::parse(&read_to_string(path)?)
    }

    /// Validates matrix data already in memory, so every rejection path is
    /// reachable without touching the filesystem.
    pub fn parse(source: &str) -> Result<Self, MatrixError> {
        let raw: RawMatrix = toml::from_str(source).map_err(|source| MatrixError::Syntax { source })?;
        if raw.matrix_version != MATRIX_SCHEMA_VERSION {
            return Err(MatrixError::UnsupportedMatrixVersion {
                version: raw.matrix_version,
                expected: MATRIX_SCHEMA_VERSION,
            });
        }
        let mut entries = Vec::with_capacity(raw.api.len());
        for (index, row) in raw.api.into_iter().enumerate() {
            let module = row.module.trim();
            if module.is_empty() {
                return Err(MatrixError::EmptyApiField {
                    index,
                    field: "module",
                });
            }
            if !DOCUMENTED_MODULES.contains(&module) {
                return Err(MatrixError::UnknownApiModule {
                    index,
                    module: module.to_owned(),
                });
            }
            let symbol = row.symbol.trim();
            if symbol.is_empty() {
                return Err(MatrixError::EmptyApiField {
                    index,
                    field: "symbol",
                });
            }
            let status = row.status.trim();
            if status.is_empty() {
                return Err(MatrixError::EmptyApiField {
                    index,
                    field: "status",
                });
            }
            let status = ApiSupport::parse_slug(status).ok_or_else(|| MatrixError::UnknownApiStatus {
                module: module.to_string(),
                symbol: symbol.to_string(),
                value: status.to_string(),
            })?;

            // A caveat nobody explained is worse than no claim at all: a plugin
            // author reading `partial` learns only that something, somewhere,
            // is different (spec 14.10, 14.12).
            let notes = row.notes.trim();
            if notes.is_empty() && matches!(status, ApiSupport::BehaviouralDifference | ApiSupport::Partial) {
                return Err(MatrixError::MissingNotes {
                    module: module.to_string(),
                    symbol: symbol.to_string(),
                    status,
                });
            }

            entries.push(MatrixEntry {
                module: module.to_string(),
                symbol: symbol.to_string(),
                status,
                notes: notes.to_string(),
            });
        }

        // Uniqueness is checked over the finished rows so the common path
        // allocates no duplicate keys; only a rejection pays for a clone.
        let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
        for entry in &entries {
            if !seen.insert((entry.module.as_str(), entry.symbol.as_str())) {
                return Err(MatrixError::DuplicateApi {
                    module: entry.module.clone(),
                    symbol: entry.symbol.clone(),
                });
            }
        }

        Ok(Self {
            version: raw.matrix_version,
            entries,
        })
    }

    /// Schema version of the loaded file. A bump is a schema change.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Every entry, in file order. The developer commands and the matrix README
    /// both present the matrix in the order a reviewer reads it.
    pub fn entries(&self) -> &[MatrixEntry] {
        &self.entries
    }

    /// Exact `(module, symbol)` lookup. `"*"` is matched literally here.
    ///
    /// Linear over the entries by design: the matrix is a few hundred rows read
    /// once, and an index would duplicate the keys while making file order a
    /// second source of truth.
    pub fn get(&self, module: &str, symbol: &str) -> Option<&MatrixEntry> {
        self.entries
            .iter()
            .find(|entry| entry.module == module && entry.symbol == symbol)
    }

    /// The support level of `symbol`, falling back to the module's `"*"` row.
    ///
    /// The exact row always wins, and a module without a `"*"` row reports
    /// `None` rather than guessing — an unclassified API must look
    /// unclassified.
    pub fn classify(&self, module: &str, symbol: &str) -> Option<ApiSupport> {
        self.get(module, symbol)
            .or_else(|| self.get(module, "*"))
            .map(|entry| entry.status)
    }

    /// Every module the matrix classifies something in, sorted and deduplicated.
    pub fn modules(&self) -> Vec<&str> {
        let unique: BTreeSet<&str> = self.entries.iter().map(|entry| entry.module.as_str()).collect();
        unique.into_iter().collect()
    }

    /// How many entries carry `status`.
    pub fn count(&self, status: ApiSupport) -> usize {
        self.entries.iter().filter(|entry| entry.status == status).count()
    }

    /// Entries that may be advertised as cross-platform (acceptance 31.31).
    /// `windows-only` is excluded by [`ApiSupport::is_portable`], not by a
    /// second rule that could drift away from it.
    pub fn portable_entries(&self) -> Vec<&MatrixEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.status.is_portable())
            .collect()
    }

    /// Entries backed by Win32 and therefore unavailable off Windows.
    pub fn windows_only_entries(&self) -> Vec<&MatrixEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.status == ApiSupport::WindowsOnly)
            .collect()
    }
}

/// The referenced corpus of real published Keypirinha packages (spec 27.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCorpus {
    version: u32,
    entries: Vec<CorpusEntry>,
}

impl PluginCorpus {
    /// Reads and validates `corpus.toml`.
    pub fn load(path: &Path) -> Result<Self, MatrixError> {
        Self::parse(&read_to_string(path)?)
    }

    /// Validates corpus data already in memory.
    pub fn parse(source: &str) -> Result<Self, MatrixError> {
        let raw: RawCorpus = toml::from_str(source).map_err(|source| MatrixError::Syntax { source })?;
        if raw.corpus_version != CORPUS_SCHEMA_VERSION {
            return Err(MatrixError::UnsupportedCorpusVersion {
                version: raw.corpus_version,
                expected: CORPUS_SCHEMA_VERSION,
            });
        }
        let mut entries = Vec::with_capacity(raw.package.len());
        for (index, row) in raw.package.into_iter().enumerate() {
            let id = row.id.trim();
            if id.is_empty() {
                return Err(MatrixError::EmptyPackageField {
                    index,
                    id: String::new(),
                    field: "id",
                });
            }
            let require = |value: &str, field: &'static str| -> Result<(), MatrixError> {
                if value.is_empty() {
                    // A missing required field means this is not a
                    // reproducible, evidenced reference.
                    Err(MatrixError::EmptyPackageField {
                        index,
                        id: id.to_string(),
                        field,
                    })
                } else {
                    Ok(())
                }
            };
            let source_url = row.source.trim();
            require(source_url, "source")?;
            if !is_valid_https_url(source_url) {
                return Err(MatrixError::InvalidPackageSource {
                    id: id.to_string(),
                    url: source_url.to_string(),
                });
            }

            let revision = row.revision.trim();
            require(revision, "revision")?;
            let licence = row.licence.trim();
            require(licence, "licence")?;
            let classification = row.classification.trim();
            require(classification, "classification")?;

            if revision.len() != REVISION_LENGTH || !revision.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(MatrixError::UnpinnedRevision {
                    id: id.to_string(),
                    revision: revision.to_string(),
                });
            }

            let classification = PluginClassification::parse_slug(classification).ok_or_else(|| {
                MatrixError::UnknownClassification {
                    package: id.to_string(),
                    value: classification.to_string(),
                }
            })?;
            let notes = row.notes.trim();
            require(notes, "notes")?;
            entries.push(CorpusEntry {
                id: id.to_string(),
                source: source_url.to_string(),
                revision: revision.to_string(),
                licence: licence.to_string(),
                classification,
                notes: notes.to_string(),
            });
        }

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for entry in &entries {
            if !seen.insert(entry.id.as_str()) {
                return Err(MatrixError::DuplicatePackage { id: entry.id.clone() });
            }
        }

        Ok(Self {
            version: raw.corpus_version,
            entries,
        })
    }

    /// Schema version of the loaded file.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Every referenced package, in file order.
    pub fn entries(&self) -> &[CorpusEntry] {
        &self.entries
    }

    /// How many packages carry `classification`.
    pub fn count(&self, classification: PluginClassification) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.classification == classification)
            .count()
    }

    /// Packages referenced but not yet exercised.
    pub fn untested(&self) -> Vec<&CorpusEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.classification == PluginClassification::Untested)
            .collect()
    }
}

/// The machine-readable compatibility summary printed by
/// `crikey dev compatibility-report`.
///
/// Counts are captured once at construction, so a report is a value a caller
/// can compare and re-render without re-reading the data files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityReport {
    matrix_total: usize,
    matrix_counts: [usize; ApiSupport::ALL.len()],
    corpus_total: usize,
    corpus_counts: [usize; PluginClassification::ALL.len()],
    corpus_portable: usize,
}

impl CompatibilityReport {
    /// Summarises one matrix and one corpus.
    pub fn new(matrix: &CompatibilityMatrix, corpus: &PluginCorpus) -> Self {
        let mut matrix_counts = [0usize; ApiSupport::ALL.len()];
        for (slot, status) in matrix_counts.iter_mut().zip(ApiSupport::ALL) {
            *slot = matrix.count(status);
        }
        let mut corpus_counts = [0usize; PluginClassification::ALL.len()];
        for (slot, class) in corpus_counts.iter_mut().zip(PluginClassification::ALL) {
            *slot = corpus.count(class);
        }

        let corpus_portable = corpus
            .entries()
            .iter()
            .filter(|entry| entry.classification.is_portable())
            .count();

        Self {
            matrix_total: matrix.entries().len(),
            matrix_counts,
            corpus_total: corpus.entries().len(),
            corpus_counts,
            corpus_portable,
        }
    }

    /// Total classified APIs.
    pub fn matrix_total(&self) -> usize {
        self.matrix_total
    }

    /// Total referenced packages.
    pub fn corpus_total(&self) -> usize {
        self.corpus_total
    }

    /// Packages whose classification permits a cross-platform claim
    /// (acceptance 31.31).
    ///
    /// Folded out of [`PluginClassification::is_portable`] rather than counted
    /// by hand, so a classification that stops asserting off-Windows operation
    /// moves this number without anyone remembering to.
    pub fn corpus_portable(&self) -> usize {
        self.corpus_portable
    }

    /// Packages that may not be presented as cross-platform: the Windows-only
    /// ones, the blocked ones, and the ones nobody has exercised.
    pub fn corpus_not_portable(&self) -> usize {
        self.corpus_total - self.corpus_portable
    }

    /// APIs classified `status`.
    pub fn matrix_count(&self, status: ApiSupport) -> usize {
        Self::index_of(&ApiSupport::ALL, status)
            .map(|index| self.matrix_counts[index])
            .unwrap_or(0)
    }

    /// Packages classified `classification`.
    pub fn corpus_count(&self, classification: PluginClassification) -> usize {
        Self::index_of(&PluginClassification::ALL, classification)
            .map(|index| self.corpus_counts[index])
            .unwrap_or(0)
    }

    fn index_of<T: PartialEq>(all: &[T], wanted: T) -> Option<usize> {
        all.iter().position(|candidate| *candidate == wanted)
    }

    /// The wire format: one `key=value` line per count, newline-terminated.
    ///
    /// Keys are derived mechanically from `ApiSupport::ALL` and
    /// [`PluginClassification::ALL`], so a new variant cannot land without a
    /// matching report key, and the order is fixed because
    /// `crikey dev compatibility-report` asserts its stdout byte for byte.
    pub fn render(&self) -> String {
        let mut out = String::new();
        push_count(&mut out, "matrix_apis", self.matrix_total);
        for (status, count) in ApiSupport::ALL.into_iter().zip(self.matrix_counts) {
            push_prefixed_count(&mut out, "matrix_", status.slug(), count);
        }
        push_count(&mut out, "corpus_plugins", self.corpus_total);
        for (class, count) in PluginClassification::ALL.into_iter().zip(self.corpus_counts) {
            push_prefixed_count(&mut out, "corpus_", class.slug(), count);
        }
        // The two portability totals close the report on the question
        // acceptance 31.31 actually asks. Without them a reader has to know
        // which of the ten classifications assert off-Windows operation, and a
        // reader who guesses wrong reads a Windows-only package as portable.
        push_count(&mut out, "corpus_portable", self.corpus_portable);
        push_count(&mut out, "corpus_not_portable", self.corpus_not_portable());
        out
    }
}

fn push_count(out: &mut String, key: &str, count: usize) {
    out.push_str(key);
    out.push('=');
    out.push_str(&count.to_string());
    out.push('\n');
}

/// Writes `<prefix><slug with '-' folded to '_'>=<count>`.
///
/// The fold is done character by character rather than with `str::replace` so
/// rendering a report allocates only its own buffer.
fn push_prefixed_count(out: &mut String, prefix: &str, slug: &str, count: usize) {
    out.push_str(prefix);
    for ch in slug.chars() {
        out.push(if ch == '-' { '_' } else { ch });
    }
    out.push('=');
    out.push_str(&count.to_string());
    out.push('\n');
}

/// Accepts an HTTPS URL only when it has a non-empty authority. A prefix check
/// alone would accept `https://`, which is not a usable repository reference.
fn is_valid_https_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty()
        && !authority.starts_with(':')
        && !authority.ends_with(':')
        && !authority
            .chars()
            .any(|character| character.is_ascii_control() || character.is_whitespace())
}
