//! The compatibility matrix and the real-plugin corpus as *tested data*
//! (spec 14.2, 14.4, 14.10, 14.12, 27.4; roadmap M3 "the compatibility matrix
//! as tested data" and "the real-plugin corpus is classified and published";
//! acceptance 31.12, 31.13, 31.31).
//!
//! Spec 14.10 says the matrix "shall be version-controlled **and tested**".
//! A markdown table is version-controlled and untested: nothing stops it from
//! claiming `full` for a symbol the shim never defines, or from quietly
//! omitting an API that regressed. This file is the mechanism that makes the
//! claim falsifiable. It loads the two committed data files —
//! `compatibility/api-matrix/matrix.toml` and
//! `compatibility/real-plugin-corpus/corpus.toml`, resolved from
//! `CARGO_MANIFEST_DIR` two levels up at the workspace root — into typed Rust
//! values and then asserts the properties prose cannot enforce.
//!
//! These tests are written before the implementation. They pin a `matrix`
//! module (`src/matrix.rs`, re-exported from the crate root per the
//! orchestrator's import ruling) that does not exist yet.
//!
//! # Surface under test
//!
//! * [`CompatibilityMatrix::load`] / [`CompatibilityMatrix::parse`] —
//!   `load` reads a file and delegates to `parse`, so every validation rule
//!   below is reachable from a `&str` without touching the committed data.
//!   Negative cases therefore need no temp files and no cleanup guard.
//! * [`MatrixEntry`] — `module`, `symbol`, `status`, `notes`.
//! * `CompatibilityMatrix::get` — exact `(module, symbol)` lookup, and
//!   `CompatibilityMatrix::classify` — the same lookup with a single
//!   documented fallback to the module's `"*"` row.
//! * `ApiSupport` — the *existing* enum in `src/lib.rs`, extended with
//!   `ALL`, `slug()`, `parse_slug()` and `is_portable()`.
//! * [`PluginCorpus::load`] / [`PluginCorpus::parse`], [`CorpusEntry`],
//!   [`PluginClassification`].
//! * [`CompatibilityReport`] — the machine-readable summary shared with
//!   `crikey dev compatibility-report` (owned by the diagnostics slice).
//! * [`MatrixError`] — one typed variant per rejection. No rejection is a
//!   `String`, and no unknown spelling is silently defaulted.
//!
//! # Conventions and deliberate choices
//!
//! * **Validation happens at load time, not at assertion time.** An empty
//!   symbol, a duplicate key, an unexplained caveat and an unpinned revision
//!   are all `MatrixError`s from `parse`. The committed files are then also
//!   scanned explicitly, so a failure names the offending row rather than
//!   only reporting "the file did not load".
//! * **Deserialization must not swallow the locator.** A missing or empty
//!   required field must surface as `EmptyApiField`/`EmptyPackageField`
//!   carrying the entry index (and package id), *not* as a generic serde
//!   `Syntax` error. Implement this by deserializing into an intermediate
//!   with `#[serde(default)]` string fields and validating afterwards.
//! * **`symbol = "*"` is a literal string, not a glob.** Storage and
//!   `get()` treat it as ordinary text. Exactly one place gives it meaning:
//!   `classify(module, symbol)` tries the exact pair first and falls back to
//!   `(module, "*")`. So a `"*"` row is a catch-all *classification* for the
//!   rest of a module without becoming a pattern-matching engine, and
//!   `keypirinha_wintypes` needs one row rather than an enumeration of every
//!   Win32 name. Agreed with the `python/` shim slice.
//! * **Private shim internals are not matrix rows.** `keypirinha._set_host`,
//!   `_clear_host` and `_install_stdout_guard` exist in the shim but are
//!   CriKey plumbing, not claimed legacy API, so they are deliberately absent
//!   from [`M3_DELIVERED_APIS`].
//! * No network, no wall clock, no `#[ignore]`, no skips. Every test here
//!   runs and must pass on the Linux CI host; the matrix rows describing
//!   Win32-backed symbols are asserted as *classification* data, which is
//!   portable, not as Win32 behaviour, which is not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crikey_legacy_compat::{
    ApiSupport, CompatibilityMatrix, CompatibilityReport, CorpusEntry, MatrixEntry, MatrixError,
    PluginClassification, PluginCorpus,
};

// ---------------------------------------------------------------------------
// Locating the committed data
// ---------------------------------------------------------------------------

/// The workspace root. `CARGO_MANIFEST_DIR` is `<root>/crates/crikey-legacy-compat`,
/// so the data files live two levels up.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR should have a workspace root two levels up")
        .to_path_buf()
}

fn matrix_path() -> PathBuf {
    workspace_root().join("compatibility/api-matrix/matrix.toml")
}

fn corpus_dir() -> PathBuf {
    workspace_root().join("compatibility/real-plugin-corpus")
}

fn corpus_path() -> PathBuf {
    corpus_dir().join("corpus.toml")
}

fn load_matrix() -> CompatibilityMatrix {
    let path = matrix_path();
    CompatibilityMatrix::load(&path).unwrap_or_else(|error| panic!("{} must load: {error}", path.display()))
}

fn load_corpus() -> PluginCorpus {
    let path = corpus_path();
    PluginCorpus::load(&path).unwrap_or_else(|error| panic!("{} must load: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// Synthetic fixtures for the rejection paths
// ---------------------------------------------------------------------------

fn matrix_source(rows: &[&str]) -> String {
    let mut source = String::from("matrix-version = 1\n");
    for row in rows {
        source.push_str("\n[[api]]\n");
        source.push_str(row);
        source.push('\n');
    }
    source
}

fn corpus_source(rows: &[&str]) -> String {
    let mut source = String::from("corpus-version = 1\n");
    for row in rows {
        source.push_str("\n[[package]]\n");
        source.push_str(row);
        source.push('\n');
    }
    source
}

const GOOD_API_ROW: &str = r#"module = "keypirinha"
symbol = "Plugin.on_start"
status = "full""#;

const GOOD_PACKAGE_ROW: &str = r#"id = "example.alpha"
source = "https://github.com/example/alpha"
revision = "0123456789abcdef0123456789abcdef01234567"
licence = "MIT"
classification = "works-unchanged""#;

/// A pinned revision is exactly 40 lowercase-or-uppercase hex characters.
const PINNED_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

// ---------------------------------------------------------------------------
// The legacy API surface (spec 14.2, 14.4)
// ---------------------------------------------------------------------------

/// The documented public legacy modules the layer must provide (spec 14.2).
const DOCUMENTED_MODULES: &[&str] = &[
    "keypirinha",
    "keypirinha_util",
    "keypirinha_net",
    "keypirinha_wintypes",
];

/// Every API M3 actually ships, as confirmed by the `python/` shim slice.
///
/// This is the honesty clamp: if the shim defines a name, the matrix must
/// classify it as something other than `planned`. Adding a symbol to the shim
/// without classifying it fails [`every_api_the_m3_layer_delivers_is_classified_as_more_than_planned`].
const M3_DELIVERED_APIS: &[(&str, &str)] = &[
    // keypirinha — module level
    ("keypirinha", "KeypirinhaError"),
    ("keypirinha", "UndocumentedApiError"),
    ("keypirinha", "InvalidItemError"),
    ("keypirinha", "SettingsError"),
    ("keypirinha", "HostUnavailableError"),
    ("keypirinha", "ItemCategory"),
    ("keypirinha", "ItemArgsHint"),
    ("keypirinha", "ItemHitHint"),
    ("keypirinha", "Match"),
    ("keypirinha", "Sort"),
    ("keypirinha", "Events"),
    ("keypirinha", "Settings"),
    ("keypirinha", "CatalogItem"),
    ("keypirinha", "Plugin"),
    ("keypirinha", "name"),
    ("keypirinha", "version"),
    ("keypirinha", "version_string"),
    ("keypirinha", "should_terminate"),
    // keypirinha.Settings
    ("keypirinha", "Settings.DEFAULT_SECTION"),
    ("keypirinha", "Settings.get"),
    ("keypirinha", "Settings.get_bool"),
    ("keypirinha", "Settings.get_int"),
    ("keypirinha", "Settings.get_float"),
    ("keypirinha", "Settings.sections"),
    ("keypirinha", "Settings.keys"),
    ("keypirinha", "Settings.has"),
    // keypirinha.CatalogItem
    ("keypirinha", "CatalogItem.category"),
    ("keypirinha", "CatalogItem.label"),
    ("keypirinha", "CatalogItem.short_desc"),
    ("keypirinha", "CatalogItem.target"),
    ("keypirinha", "CatalogItem.args_hint"),
    ("keypirinha", "CatalogItem.hit_hint"),
    ("keypirinha", "CatalogItem.loop_on_suggest"),
    ("keypirinha", "CatalogItem.icon_handle"),
    ("keypirinha", "CatalogItem.data_bag"),
    ("keypirinha", "CatalogItem.set_data_bag"),
    // keypirinha.Plugin
    ("keypirinha", "Plugin.id"),
    ("keypirinha", "Plugin.friendly_name"),
    ("keypirinha", "Plugin.package_full_name"),
    ("keypirinha", "Plugin.on_start"),
    ("keypirinha", "Plugin.on_catalog"),
    ("keypirinha", "Plugin.on_suggest"),
    ("keypirinha", "Plugin.on_execute"),
    ("keypirinha", "Plugin.on_activated"),
    ("keypirinha", "Plugin.on_deactivated"),
    ("keypirinha", "Plugin.on_events"),
    ("keypirinha", "Plugin.create_item"),
    ("keypirinha", "Plugin.set_catalog"),
    ("keypirinha", "Plugin.merge_catalog"),
    ("keypirinha", "Plugin.set_suggestions"),
    ("keypirinha", "Plugin.should_terminate"),
    ("keypirinha", "Plugin.load_settings"),
    ("keypirinha", "Plugin.package_full_path"),
    ("keypirinha", "Plugin.get_package_cache_path"),
    ("keypirinha", "Plugin.load_text_resource"),
    ("keypirinha", "Plugin.load_binary_resource"),
    ("keypirinha", "Plugin.info"),
    ("keypirinha", "Plugin.warn"),
    ("keypirinha", "Plugin.err"),
    ("keypirinha", "Plugin.dbg"),
    // keypirinha_util. `set_clipboard`, `get_clipboard`, `open_url`,
    // `shell_execute` and `explore_file` are *not* windows-only: they are
    // `partial`, because they need a desktop session and raise
    // `keypirinha_util.UnavailableError` on a headless host.
    ("keypirinha_util", "UnavailableError"),
    ("keypirinha_util", "ScanFlags"),
    ("keypirinha_util", "cmdline_split"),
    ("keypirinha_util", "cmdline_quote"),
    ("keypirinha_util", "expand_variables"),
    ("keypirinha_util", "scan_directory"),
    ("keypirinha_util", "desktop_available"),
    ("keypirinha_util", "set_clipboard"),
    ("keypirinha_util", "get_clipboard"),
    ("keypirinha_util", "open_url"),
    ("keypirinha_util", "shell_execute"),
    ("keypirinha_util", "explore_file"),
    // keypirinha_net
    ("keypirinha_net", "InvalidUrlError"),
    ("keypirinha_net", "Request"),
    ("keypirinha_net", "DEFAULT_TIMEOUT"),
    ("keypirinha_net", "user_agent"),
    ("keypirinha_net", "build_request"),
    ("keypirinha_net", "build_urllib_opener"),
    ("keypirinha_net", "Request.url"),
    ("keypirinha_net", "Request.headers"),
    ("keypirinha_net", "Request.timeout"),
    ("keypirinha_net", "Request.user_agent"),
    ("keypirinha_net", "Request.get_header"),
    // keypirinha_wintypes. These four resolve normally on Linux — they are
    // the honest-unavailability surface itself, not Win32 calls — so they are
    // classified on their own merits. Everything else in the module falls
    // through the `"*"` row.
    ("keypirinha_wintypes", "WINDOWS_ONLY"),
    ("keypirinha_wintypes", "WINDOWS_ONLY_SYMBOLS"),
    ("keypirinha_wintypes", "WindowsOnlyError"),
    ("keypirinha_wintypes", "is_available"),
    ("keypirinha_wintypes", "*"),
];

/// Documented Keypirinha APIs M3 deliberately does *not* ship.
///
/// They must still appear in the matrix: spec 14.10 classifies *each*
/// documented legacy API, and silently omitting a gap is exactly the
/// dishonesty this file exists to prevent. `fuzzy_score` is the canonical
/// case — spec 14.12 exempts "exact reproduction of undocumented ranking
/// behavior", so it is a documented non-goal, not an oversight.
const M3_DEFERRED_APIS: &[(&str, &str)] = &[
    ("keypirinha", "user_config_dir"),
    ("keypirinha", "installed_package_dir"),
    ("keypirinha", "package_cache_dir"),
    ("keypirinha_util", "fuzzy_score"),
    ("keypirinha_util", "chardet_open"),
    ("keypirinha_util", "decode_bytes"),
    ("keypirinha_util", "kwargs_encode"),
    ("keypirinha_util", "kwargs_decode"),
    ("keypirinha_util", "execute_default_action"),
    ("keypirinha_util", "web_browser_command"),
    ("keypirinha_util", "read_link"),
];

/// The names in the shim's `keypirinha_wintypes.WINDOWS_ONLY_SYMBOLS`.
///
/// Attribute access to each raises `WindowsOnlyError` on Linux: they are
/// backed by Win32 and cannot resolve off Windows. The matrix is not required
/// to enumerate them — the module's `"*"` row may cover them — but
/// `classify()` must report every one of them as `windows-only`, and none may
/// ever surface through the portability query (acceptance 31.31).
const WIN32_BACKED_SYMBOLS: &[(&str, &str)] = &[
    ("keypirinha_wintypes", "kernel32"),
    ("keypirinha_wintypes", "user32"),
    ("keypirinha_wintypes", "shell32"),
    ("keypirinha_wintypes", "ole32"),
    ("keypirinha_wintypes", "declare_func"),
    ("keypirinha_wintypes", "GUID"),
];

/// Corpus classifications the project actually claims, and must therefore
/// evidence with at least one real package.
///
/// * `works-unchanged` and `works-with-minimal-source-changes` — acceptance
///   31.13 ("works unchanged or with documented minimal changes").
/// * `works-with-configuration-changes` — spec 14.1 ("only minimal source or
///   packaging changes").
/// * `windows-only-but-compatible` — acceptance 31.31 requires the project to
///   be able to *say* a plugin is Windows-only, which is vacuous unless the
///   corpus contains one.
///
/// The other five §27.4 classifications describe failures. The project does
/// not claim them, so the corpus is not required to contain one; if it does,
/// the report counts it like any other.
const CLAIMED_CLASSIFICATIONS: &[PluginClassification] = &[
    PluginClassification::WorksUnchanged,
    PluginClassification::WorksWithConfigurationChanges,
    PluginClassification::WorksWithMinimalSourceChanges,
    PluginClassification::WindowsOnlyButCompatible,
];

/// The wire contract for the machine-readable report, agreed with the
/// diagnostics slice which prints it from `crikey dev compatibility-report`.
/// Order is fixed and load-bearing: that command asserts its stdout
/// byte-for-byte.
const REPORT_KEYS: &[&str] = &[
    "matrix_apis",
    "matrix_full",
    "matrix_behavioural_difference",
    "matrix_windows_only",
    "matrix_partial",
    "matrix_unsupported",
    "matrix_planned",
    "corpus_plugins",
    "corpus_works_unchanged",
    "corpus_works_with_configuration_changes",
    "corpus_works_with_minimal_source_changes",
    "corpus_windows_only_but_compatible",
    "corpus_blocked_missing_apis",
    "corpus_blocked_python_version",
    "corpus_blocked_undocumented_behaviour",
    "corpus_works_only_under_legacy_optimized",
    "corpus_requires_legacy_strict",
    "corpus_untested",
    // The two portability totals of acceptance 31.31. Folded out of
    // `PluginClassification::is_portable` rather than out of a slug, so they are
    // listed literally instead of derived from `ALL` below.
    "corpus_portable",
    "corpus_not_portable",
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn describe(entry: &MatrixEntry) -> String {
    format!("{}::{}", entry.module, entry.symbol)
}

fn parse_report(rendered: &str) -> Vec<(String, u64)> {
    rendered
        .lines()
        .map(|line| {
            let (key, value) = line
                .split_once('=')
                .unwrap_or_else(|| panic!("report line {line:?} must be `key=value`"));
            let count = value
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("report value for {key:?} must be a count, got {value:?}"));
            (key.to_string(), count)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The committed files parse into typed data
// ---------------------------------------------------------------------------

#[test]
fn the_committed_compatibility_matrix_parses_into_typed_entries() {
    let matrix = load_matrix();

    assert_eq!(
        matrix.version(),
        1,
        "matrix.toml declares matrix-version = 1; a bump is a schema change that must update this suite"
    );
    assert!(
        !matrix.entries().is_empty(),
        "matrix.toml must classify at least one API (spec 14.10)"
    );

    // Every status resolved to a typed variant. `parse_slug` is total over the
    // six §14.10 classes and returns `None` for anything else, so a row that
    // survived `load` cannot be holding a defaulted status.
    for entry in matrix.entries() {
        let slug = entry.status.slug();
        assert_eq!(
            ApiSupport::parse_slug(slug),
            Some(entry.status),
            "{} resolved to a status whose slug {slug:?} does not round-trip",
            describe(entry)
        );
    }

    // The per-status counts partition the matrix: nothing is uncounted.
    let counted: usize = ApiSupport::ALL.iter().map(|s| matrix.count(*s)).sum();
    assert_eq!(
        counted,
        matrix.entries().len(),
        "every matrix entry must fall into exactly one of the six §14.10 statuses"
    );
}

#[test]
fn the_committed_plugin_corpus_parses_into_typed_entries() {
    let corpus = load_corpus();

    assert_eq!(
        corpus.version(),
        1,
        "corpus.toml declares corpus-version = 1; a bump is a schema change that must update this suite"
    );
    assert!(
        !corpus.entries().is_empty(),
        "the real-plugin corpus must reference at least one package (spec 27.4, roadmap M3 exit criteria)"
    );

    for entry in corpus.entries() {
        let slug = entry.classification.slug();
        assert_eq!(
            PluginClassification::parse_slug(slug),
            Some(entry.classification),
            "package {} resolved to a classification whose slug {slug:?} does not round-trip",
            entry.id
        );
    }

    let counted: usize = PluginClassification::ALL.iter().map(|c| corpus.count(*c)).sum();
    assert_eq!(
        counted,
        corpus.entries().len(),
        "every corpus package must fall into exactly one classification"
    );
}

// ---------------------------------------------------------------------------
// Unknown spellings are typed errors, never silent defaults
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_api_status_spelling_is_a_typed_error_naming_the_offending_value() {
    let source = matrix_source(&[r#"module = "keypirinha"
symbol = "Plugin.on_suggest"
status = "mostly-fine""#]);

    let error = CompatibilityMatrix::parse(&source)
        .expect_err("an unrecognised status must not be defaulted to any variant");

    match &error {
        MatrixError::UnknownApiStatus {
            module,
            symbol,
            value,
        } => {
            assert_eq!(module, "keypirinha");
            assert_eq!(symbol, "Plugin.on_suggest");
            assert_eq!(value, "mostly-fine");
        }
        other => panic!("expected MatrixError::UnknownApiStatus, got {other:?}"),
    }

    let rendered = error.to_string();
    assert!(
        rendered.contains("mostly-fine") && rendered.contains("Plugin.on_suggest"),
        "the error message must name the offending value and entry, got {rendered:?}"
    );
}

#[test]
fn an_unknown_corpus_classification_spelling_is_a_typed_error_naming_the_offending_value() {
    let source = corpus_source(&[&format!(
        r#"id = "example.beta"
source = "https://github.com/example/beta"
revision = "{PINNED_REVISION}"
licence = "MIT"
classification = "probably-ok""#
    )]);

    let error = PluginCorpus::parse(&source)
        .expect_err("an unrecognised classification must not be defaulted to any variant");

    match &error {
        MatrixError::UnknownClassification { package, value } => {
            assert_eq!(package, "example.beta");
            assert_eq!(value, "probably-ok");
        }
        other => panic!("expected MatrixError::UnknownClassification, got {other:?}"),
    }

    let rendered = error.to_string();
    assert!(
        rendered.contains("probably-ok") && rendered.contains("example.beta"),
        "the error message must name the offending value and package, got {rendered:?}"
    );
}

#[test]
fn every_status_and_classification_slug_is_distinct_and_round_trips() {
    // A total, injective slug vocabulary is what lets "unknown spelling" be a
    // decidable error rather than a judgement call.
    let statuses: BTreeSet<&str> = ApiSupport::ALL.iter().map(|s| s.slug()).collect();
    assert_eq!(
        statuses.len(),
        ApiSupport::ALL.len(),
        "two ApiSupport variants share a slug: {statuses:?}"
    );
    for status in ApiSupport::ALL {
        assert_eq!(
            ApiSupport::parse_slug(status.slug()),
            Some(status),
            "{status:?} does not round-trip through its slug {:?}",
            status.slug()
        );
    }

    let classes: BTreeSet<&str> = PluginClassification::ALL.iter().map(|c| c.slug()).collect();
    assert_eq!(
        classes.len(),
        PluginClassification::ALL.len(),
        "two PluginClassification variants share a slug: {classes:?}"
    );
    for class in PluginClassification::ALL {
        assert_eq!(
            PluginClassification::parse_slug(class.slug()),
            Some(class),
            "{class:?} does not round-trip through its slug {:?}",
            class.slug()
        );
    }

    assert_eq!(ApiSupport::parse_slug(""), None);
    assert_eq!(
        ApiSupport::parse_slug("Full"),
        None,
        "slugs are kebab-case and case sensitive"
    );
    assert_eq!(PluginClassification::parse_slug(""), None);
    assert_eq!(
        PluginClassification::parse_slug("works_unchanged"),
        None,
        "slugs are kebab-case, not snake_case"
    );
}

// ---------------------------------------------------------------------------
// Well-formedness of entries
// ---------------------------------------------------------------------------

#[test]
fn every_committed_entry_carries_its_identifying_fields() {
    let matrix = load_matrix();
    for (index, entry) in matrix.entries().iter().enumerate() {
        assert!(
            !entry.module.trim().is_empty(),
            "matrix api[{index}] has an empty module"
        );
        assert!(
            !entry.symbol.trim().is_empty(),
            "matrix api[{index}] (module {:?}) has an empty symbol",
            entry.module
        );
    }

    let corpus = load_corpus();
    for (index, entry) in corpus.entries().iter().enumerate() {
        let CorpusEntry {
            id,
            source,
            revision,
            licence,
            ..
        } = entry;
        assert!(!id.trim().is_empty(), "corpus package[{index}] has an empty id");
        assert!(
            !source.trim().is_empty(),
            "corpus package[{index}] ({id}) has an empty source"
        );
        assert!(
            !revision.trim().is_empty(),
            "corpus package[{index}] ({id}) has an empty revision"
        );
        assert!(
            !licence.trim().is_empty(),
            "corpus package[{index}] ({id}) has an empty licence; an unlicensed reference cannot be redistributed or reproduced"
        );
    }
}

#[test]
fn an_empty_module_or_symbol_is_rejected_with_a_typed_error() {
    let missing_module = matrix_source(&[r#"module = ""
symbol = "Plugin.on_start"
status = "full""#]);
    match CompatibilityMatrix::parse(&missing_module)
        .expect_err("an entry with no module identifies nothing and must be rejected")
    {
        MatrixError::EmptyApiField { index, field } => {
            assert_eq!(index, 0);
            assert_eq!(field, "module");
        }
        other => panic!("expected MatrixError::EmptyApiField for module, got {other:?}"),
    }

    // A row that omits the key entirely must produce the same located error,
    // not an anonymous deserialization failure.
    let absent_symbol = matrix_source(&[
        GOOD_API_ROW,
        r#"module = "keypirinha_net"
status = "full""#,
    ]);
    match CompatibilityMatrix::parse(&absent_symbol)
        .expect_err("an entry with no symbol identifies nothing and must be rejected")
    {
        MatrixError::EmptyApiField { index, field } => {
            assert_eq!(
                index, 1,
                "the error must locate the offending row, not the first row"
            );
            assert_eq!(field, "symbol");
        }
        other => panic!("expected MatrixError::EmptyApiField for symbol, got {other:?}"),
    }

    let absent_licence = corpus_source(&[&format!(
        r#"id = "example.gamma"
source = "https://github.com/example/gamma"
revision = "{PINNED_REVISION}"
classification = "works-unchanged""#
    )]);
    match PluginCorpus::parse(&absent_licence)
        .expect_err("a corpus reference without a licence must be rejected")
    {
        MatrixError::EmptyPackageField { index, id, field } => {
            assert_eq!(index, 0);
            assert_eq!(id, "example.gamma");
            assert_eq!(field, "licence");
        }
        other => panic!("expected MatrixError::EmptyPackageField for licence, got {other:?}"),
    }
}

#[test]
fn duplicate_keys_are_rejected_with_a_typed_error() {
    let duplicated_api = matrix_source(&[
        GOOD_API_ROW,
        r#"module = "keypirinha"
symbol = "Plugin.on_start"
status = "partial"
notes = "second, contradictory claim""#,
    ]);
    match CompatibilityMatrix::parse(&duplicated_api).expect_err(
        "two rows for one (module, symbol) make the classification ambiguous and must be rejected",
    ) {
        MatrixError::DuplicateApi { module, symbol } => {
            assert_eq!(module, "keypirinha");
            assert_eq!(symbol, "Plugin.on_start");
        }
        other => panic!("expected MatrixError::DuplicateApi, got {other:?}"),
    }

    let duplicated_package = corpus_source(&[GOOD_PACKAGE_ROW, GOOD_PACKAGE_ROW]);
    match PluginCorpus::parse(&duplicated_package)
        .expect_err("two rows for one package id make the classification ambiguous")
    {
        MatrixError::DuplicatePackage { id } => assert_eq!(id, "example.alpha"),
        other => panic!("expected MatrixError::DuplicatePackage, got {other:?}"),
    }
}

#[test]
fn the_committed_files_contain_no_duplicate_keys() {
    let matrix = load_matrix();
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for entry in matrix.entries() {
        assert!(
            seen.insert((entry.module.as_str(), entry.symbol.as_str())),
            "matrix.toml classifies {} twice",
            describe(entry)
        );
    }

    let corpus = load_corpus();
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    for entry in corpus.entries() {
        assert!(
            ids.insert(entry.id.as_str()),
            "corpus.toml lists package {} twice",
            entry.id
        );
    }
}

// ---------------------------------------------------------------------------
// Coverage of the documented surface
// ---------------------------------------------------------------------------

#[test]
fn every_documented_public_legacy_module_is_represented_in_the_matrix() {
    let matrix = load_matrix();
    let present: BTreeSet<&str> = matrix.modules().into_iter().collect();

    for module in DOCUMENTED_MODULES {
        assert!(
            present.contains(module),
            "spec 14.2 requires the layer to provide `{module}`, but matrix.toml classifies nothing in it (present: {present:?})"
        );
    }

    // The converse: a module in the matrix that is not a documented legacy
    // module is either a typo or an undeclared surface (spec 14.12).
    for module in &present {
        assert!(
            DOCUMENTED_MODULES.contains(module),
            "matrix.toml classifies module `{module}`, which is not one of the documented public modules {DOCUMENTED_MODULES:?} (spec 14.2)"
        );
    }
}

#[test]
fn every_api_the_m3_layer_delivers_is_classified_as_more_than_planned() {
    let matrix = load_matrix();

    let mut missing: Vec<String> = Vec::new();
    let mut still_planned: Vec<String> = Vec::new();

    for (module, symbol) in M3_DELIVERED_APIS {
        match matrix.get(module, symbol) {
            None => missing.push(format!("{module}::{symbol}")),
            Some(entry) => {
                if entry.status == ApiSupport::Planned {
                    still_planned.push(format!("{module}::{symbol}"));
                }
            }
        }
    }

    assert!(
        missing.is_empty(),
        "the M3 shim defines these symbols but matrix.toml does not classify them, so the published matrix understates the layer: {missing:#?}"
    );
    assert!(
        still_planned.is_empty(),
        "these symbols are shipped by the M3 shim but matrix.toml still calls them `planned`, so the published matrix is stale: {still_planned:#?}"
    );
}

#[test]
fn apis_the_m3_layer_does_not_deliver_are_classified_planned_or_unsupported() {
    let matrix = load_matrix();

    for (module, symbol) in M3_DEFERRED_APIS {
        let entry = matrix.get(module, symbol).unwrap_or_else(|| {
            panic!(
                "{module}::{symbol} is a documented Keypirinha API that M3 does not ship; spec 14.10 requires it to be classified rather than omitted"
            )
        });
        assert!(
            matches!(entry.status, ApiSupport::Planned | ApiSupport::Unsupported),
            "{module}::{symbol} is not implemented by the M3 shim but matrix.toml advertises it as {:?}",
            entry.status
        );
    }
}

// ---------------------------------------------------------------------------
// Windows-only entries are never advertised as portable (acceptance 31.31)
// ---------------------------------------------------------------------------

#[test]
fn every_win32_backed_symbol_is_classified_windows_only() {
    let matrix = load_matrix();

    // The catch-all row must exist and must be the strict answer, otherwise
    // the fallback below would be classifying against nothing.
    let wildcard = matrix
        .get("keypirinha_wintypes", "*")
        .expect("matrix.toml must carry a `keypirinha_wintypes` `*` row (spec 14.2)");
    assert_eq!(
        wildcard.status,
        ApiSupport::WindowsOnly,
        "the keypirinha_wintypes catch-all is Win32-backed; classifying it {:?} would advertise the module off Windows",
        wildcard.status
    );

    // Each name in the shim's WINDOWS_ONLY_SYMBOLS must resolve to
    // `windows-only`, whether the matrix spells it out or leaves it to `*`.
    for (module, symbol) in WIN32_BACKED_SYMBOLS {
        let status = matrix.classify(module, symbol).unwrap_or_else(|| {
            panic!(
                "{module}::{symbol} raises WindowsOnlyError on this host, so the matrix must classify it either directly or through the module's `*` row"
            )
        });
        assert_eq!(
            status,
            ApiSupport::WindowsOnly,
            "{module}::{symbol} resolves only against Win32, so classifying it {status:?} would advertise it off Windows"
        );
        assert!(
            !status.is_portable(),
            "{module}::{symbol} must never be reported as portable (acceptance 31.31)"
        );
    }

    // The fallback is a fallback, not a wildcard match: an exact row always
    // wins, and a module with no `*` row reports nothing rather than guessing.
    assert_eq!(
        matrix.classify("keypirinha_wintypes", "is_available"),
        Some(ApiSupport::Full),
        "an exact row must take precedence over the module's `*` row"
    );
    assert_eq!(
        matrix.classify("keypirinha", "no_such_symbol"),
        None,
        "`keypirinha` has no `*` row, so an unknown symbol must classify as nothing rather than defaulting"
    );
    assert_eq!(
        matrix.classify("not_a_module", "*"),
        None,
        "an unknown module must classify as nothing"
    );
}

#[test]
fn nothing_classified_windows_only_is_advertised_as_portable() {
    let matrix = load_matrix();

    let windows_only = matrix.windows_only_entries();
    assert!(
        !windows_only.is_empty(),
        "matrix.toml must classify at least the Win32-backed keypirinha_wintypes surface as windows-only (spec 14.2)"
    );

    let portable = matrix.portable_entries();
    for entry in &windows_only {
        assert!(
            !entry.status.is_portable(),
            "{} is windows-only, so ApiSupport::is_portable must be false",
            describe(entry)
        );
        assert!(
            !portable
                .iter()
                .any(|p| p.module == entry.module && p.symbol == entry.symbol),
            "{} is windows-only but is returned by the portability query, which would present it as cross-platform (acceptance 31.31)",
            describe(entry)
        );
    }

    for entry in &portable {
        assert_ne!(
            entry.status,
            ApiSupport::WindowsOnly,
            "the portability query leaked windows-only entry {}",
            describe(entry)
        );
        assert!(
            entry.status.is_portable(),
            "the portability query returned {} whose status {:?} is not portable",
            describe(entry),
            entry.status
        );
    }

    // The two queries never overlap and never together over-count the matrix.
    assert!(
        portable.len() + windows_only.len() <= matrix.entries().len(),
        "the portable and windows-only queries must be disjoint subsets of the matrix"
    );
}

// ---------------------------------------------------------------------------
// Caveats must explain themselves
// ---------------------------------------------------------------------------

#[test]
fn an_unexplained_caveat_is_rejected_with_a_typed_error_naming_the_entry() {
    // Absent notes and whitespace-only notes are the same failure: a reader
    // learns nothing about what actually differs.
    let empty_notes = ["", "notes = \"\"\n", "notes = \"   \"\n"];

    for slug in ["behavioural-difference", "partial"] {
        let expected = ApiSupport::parse_slug(slug).expect("fixture slug must be a known status");

        for notes in empty_notes {
            let source = matrix_source(&[&format!(
                "module = \"keypirinha_util\"\nsymbol = \"shell_execute\"\nstatus = \"{slug}\"\n{notes}"
            )]);

            let error = CompatibilityMatrix::parse(&source).expect_err(&format!(
                "a `{slug}` caveat with notes {notes:?} explains nothing and must be rejected"
            ));

            match &error {
                MatrixError::MissingNotes {
                    module,
                    symbol,
                    status,
                } => {
                    assert_eq!(module, "keypirinha_util");
                    assert_eq!(symbol, "shell_execute");
                    assert_eq!(*status, expected);
                }
                other => panic!("expected MatrixError::MissingNotes for {slug}, got {other:?}"),
            }

            let rendered = error.to_string();
            assert!(
                rendered.contains("shell_execute"),
                "the error must name the offending entry, got {rendered:?}"
            );
        }
    }

    // A caveat that does explain itself is accepted.
    let explained = matrix_source(&[r#"module = "keypirinha_util"
symbol = "shell_execute"
status = "partial"
notes = "requires a desktop session; raises UnavailableError on a headless host""#]);
    let matrix = CompatibilityMatrix::parse(&explained).expect("a caveat with real notes must be accepted");
    assert_eq!(matrix.entries().len(), 1);
}

#[test]
fn every_caveated_entry_in_the_committed_matrix_explains_the_difference() {
    let matrix = load_matrix();

    let caveated: Vec<&MatrixEntry> = matrix
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                ApiSupport::BehaviouralDifference | ApiSupport::Partial
            )
        })
        .collect();

    for entry in caveated {
        assert!(
            !entry.notes.trim().is_empty(),
            "{} is classified {:?} but carries no notes; an unexplained caveat tells a plugin author nothing (spec 14.10, 14.12)",
            describe(entry),
            entry.status
        );
    }
}

// ---------------------------------------------------------------------------
// The corpus is reproducible and unvendored
// ---------------------------------------------------------------------------

#[test]
fn every_corpus_revision_is_pinned_to_a_full_commit_hash() {
    let corpus = load_corpus();

    for entry in corpus.entries() {
        let revision = &entry.revision;
        assert_eq!(
            revision.len(),
            40,
            "package {} pins revision {revision:?}, which is {} characters; a corpus result is only reproducible against a full 40-character commit hash",
            entry.id,
            revision.len()
        );
        assert!(
            revision.chars().all(|c| c.is_ascii_hexdigit()),
            "package {} pins revision {revision:?}, which is not hexadecimal; branch names and tags move and cannot reproduce a classification",
            entry.id
        );
    }
}

#[test]
fn an_unpinned_corpus_revision_is_rejected_with_a_typed_error() {
    for revision in ["main", "v1.2.3", "0123456", ""] {
        let source = corpus_source(&[&format!(
            r#"id = "example.delta"
source = "https://github.com/example/delta"
revision = "{revision}"
licence = "MIT"
classification = "works-unchanged""#
        )]);

        let error = PluginCorpus::parse(&source).unwrap_err();
        match (&error, revision) {
            (MatrixError::EmptyPackageField { field, .. }, "") => {
                assert_eq!(*field, "revision");
            }
            (MatrixError::UnpinnedRevision { id, revision: got }, _) => {
                assert_eq!(id, "example.delta");
                assert_eq!(got, revision);
                assert!(
                    error.to_string().contains(revision),
                    "the error must name the offending revision {revision:?}"
                );
            }
            (other, _) => panic!("revision {revision:?} must be rejected as unpinned, got {other:?}"),
        }
    }
}

#[test]
fn the_corpus_references_packages_and_vendors_none_of_them() {
    // Spec 27.4 and the corpus README: packages are referenced, never
    // vendored. Vendoring third-party plugin source into this repository would
    // import their licences and let a classification drift away from the
    // upstream revision it claims to describe.
    const ALLOWED: &[&str] = &["corpus.toml", "README.md"];

    let dir = corpus_dir();
    let mut unexpected: Vec<String> = Vec::new();

    let listing =
        std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("{} must exist: {error}", dir.display()));
    for entry in listing {
        let entry = entry.expect("directory entry must be readable");
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().expect("file type must be readable");

        if file_type.is_dir() {
            unexpected.push(format!("{name}/ (directory)"));
        } else if !ALLOWED.contains(&name.as_str()) {
            unexpected.push(name);
        }
    }

    unexpected.sort();
    assert!(
        unexpected.is_empty(),
        "{} must contain only {ALLOWED:?}; packages are referenced by source + pinned revision, never vendored. Found: {unexpected:?}",
        dir.display()
    );

    // Each reference must actually be a reference: a resolvable remote source,
    // not a path into this tree.
    let corpus = load_corpus();
    for entry in corpus.entries() {
        assert!(
            entry.source.starts_with("https://"),
            "package {} declares source {:?}; a corpus reference must be an https URL to the upstream package",
            entry.id,
            entry.source
        );
    }
}

// ---------------------------------------------------------------------------
// Honest coverage
// ---------------------------------------------------------------------------

#[test]
fn the_corpus_evidences_every_classification_the_project_claims() {
    let corpus = load_corpus();

    for claimed in CLAIMED_CLASSIFICATIONS {
        let matching: Vec<&str> = corpus
            .entries()
            .iter()
            .filter(|entry| entry.classification == *claimed)
            .map(|entry| entry.id.as_str())
            .collect();
        assert!(
            !matching.is_empty(),
            "the project claims `{}` (acceptance 31.13, 31.31; spec 14.1) but no corpus package is classified that way, so the claim is unevidenced",
            claimed.slug()
        );
        assert_eq!(
            corpus.count(*claimed),
            matching.len(),
            "count({claimed:?}) disagrees with the entries carrying that classification: {matching:?}"
        );
    }
}

#[test]
fn untested_corpus_entries_are_permitted_but_counted() {
    let corpus = load_corpus();

    // `untested` is a legitimate, honest state: it says "we reference this
    // package and have not run it yet". What it may never do is disappear from
    // the published totals, which would overstate coverage.
    let untested = corpus.untested();
    assert_eq!(
        untested.len(),
        corpus.count(PluginClassification::Untested),
        "untested() and count(Untested) must agree"
    );
    for entry in &untested {
        assert_eq!(entry.classification, PluginClassification::Untested);
        assert!(
            !CLAIMED_CLASSIFICATIONS.contains(&entry.classification),
            "an untested package may never satisfy a coverage claim"
        );
    }

    let report = CompatibilityReport::new(&load_matrix(), &corpus);
    assert_eq!(
        report.corpus_count(PluginClassification::Untested),
        untested.len(),
        "the published report must count untested packages, not omit them"
    );

    let tested = corpus.entries().len() - untested.len();
    assert_eq!(
        tested + untested.len(),
        report.corpus_total(),
        "tested + untested must account for every referenced package"
    );
}

// ---------------------------------------------------------------------------
// The machine-readable report
// ---------------------------------------------------------------------------

#[test]
fn the_report_counts_every_matrix_and_corpus_entry_exactly_once() {
    let matrix = load_matrix();
    let corpus = load_corpus();
    let report = CompatibilityReport::new(&matrix, &corpus);

    assert_eq!(report.matrix_total(), matrix.entries().len());
    assert_eq!(report.corpus_total(), corpus.entries().len());

    let matrix_sum: usize = ApiSupport::ALL
        .iter()
        .map(|status| {
            let count = report.matrix_count(*status);
            assert_eq!(
                count,
                matrix.count(*status),
                "report and matrix disagree on the number of {status:?} entries"
            );
            count
        })
        .sum();
    assert_eq!(
        matrix_sum,
        report.matrix_total(),
        "the six §14.10 status counts must sum to matrix_apis"
    );

    let corpus_sum: usize = PluginClassification::ALL
        .iter()
        .map(|class| {
            let count = report.corpus_count(*class);
            assert_eq!(
                count,
                corpus.count(*class),
                "report and corpus disagree on the number of {class:?} packages"
            );
            count
        })
        .sum();
    assert_eq!(
        corpus_sum,
        report.corpus_total(),
        "the ten classification counts must sum to corpus_plugins"
    );
}

#[test]
fn the_report_renders_the_agreed_keys_in_a_fixed_order() {
    let report = CompatibilityReport::new(&load_matrix(), &load_corpus());
    let rendered = report.render();

    assert!(
        rendered.ends_with('\n'),
        "render() must terminate its final line so the CLI can concatenate without fixups"
    );

    let lines = parse_report(&rendered);
    let keys: Vec<&str> = lines.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys, REPORT_KEYS,
        "render() must emit exactly the agreed keys, in order; `crikey dev compatibility-report` asserts its stdout byte-for-byte"
    );

    // The wire keys are mechanically derived from the typed vocabulary, so a
    // new enum variant cannot land without a matching report key.
    let mut derived = vec!["matrix_apis".to_string()];
    derived.extend(
        ApiSupport::ALL
            .iter()
            .map(|s| format!("matrix_{}", s.slug().replace('-', "_"))),
    );
    derived.push("corpus_plugins".to_string());
    derived.extend(
        PluginClassification::ALL
            .iter()
            .map(|c| format!("corpus_{}", c.slug().replace('-', "_"))),
    );
    derived.push("corpus_portable".to_string());
    derived.push("corpus_not_portable".to_string());
    assert_eq!(
        derived, REPORT_KEYS,
        "the report keys must stay mechanically derivable from ApiSupport::ALL and PluginClassification::ALL"
    );

    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for (key, value) in &lines {
        assert!(
            counts.insert(key.as_str(), *value).is_none(),
            "render() emitted duplicate key {key:?}"
        );
    }
    assert_eq!(
        counts["matrix_apis"],
        report.matrix_total() as u64,
        "matrix_apis must match matrix_total()"
    );
    assert_eq!(
        counts["corpus_plugins"],
        report.corpus_total() as u64,
        "corpus_plugins must match corpus_total()"
    );
}

#[test]
fn the_report_is_byte_identical_across_repeated_loads() {
    // Determinism is what makes the report reviewable in a diff. Iterating a
    // `HashMap` anywhere in the pipeline would break this.
    let first = CompatibilityReport::new(&load_matrix(), &load_corpus());
    let second = CompatibilityReport::new(&load_matrix(), &load_corpus());

    assert_eq!(
        first, second,
        "two independent loads of the same files must produce equal reports"
    );
    assert_eq!(
        first.render(),
        second.render(),
        "two independent loads of the same files must render byte-identically"
    );
    assert_eq!(
        first.render(),
        first.render(),
        "render() must be pure: repeated calls on one report must agree"
    );

    // Entry order is a property of the data too: the diagnostics CLI and the
    // matrix README both present entries in file order.
    let a = load_matrix();
    let b = load_matrix();
    assert_eq!(a.entries(), b.entries(), "matrix entry order must be stable");
    let c = load_corpus();
    let d = load_corpus();
    assert_eq!(c.entries(), d.entries(), "corpus entry order must be stable");
}

// ---------------------------------------------------------------------------
// I/O and syntax failures stay typed
// ---------------------------------------------------------------------------

#[test]
fn a_missing_data_file_is_a_typed_io_error_naming_the_path() {
    let absent = std::env::temp_dir().join("crikey-compat-matrix-does-not-exist-6d1f4a.toml");
    assert!(
        !absent.exists(),
        "test precondition: {} must not exist",
        absent.display()
    );

    match CompatibilityMatrix::load(&absent).expect_err("loading an absent matrix must fail") {
        MatrixError::Io { path, .. } => assert_eq!(path, absent),
        other => panic!("expected MatrixError::Io, got {other:?}"),
    }

    match PluginCorpus::load(&absent).expect_err("loading an absent corpus must fail") {
        MatrixError::Io { path, .. } => assert_eq!(path, absent),
        other => panic!("expected MatrixError::Io, got {other:?}"),
    }
}

#[test]
fn malformed_toml_is_a_typed_syntax_error() {
    let error = CompatibilityMatrix::parse("matrix-version = = 1\n[[api]\n")
        .expect_err("malformed TOML must not be recovered from");
    assert!(
        matches!(error, MatrixError::Syntax { .. }),
        "expected MatrixError::Syntax, got {error:?}"
    );

    let error = PluginCorpus::parse("corpus-version = \n[[package]\n")
        .expect_err("malformed TOML must not be recovered from");
    assert!(
        matches!(error, MatrixError::Syntax { .. }),
        "expected MatrixError::Syntax, got {error:?}"
    );
}
