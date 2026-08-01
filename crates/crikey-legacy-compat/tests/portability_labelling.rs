//! Windows-only legacy plugins are labelled as such, reached only through an
//! explicit platform interface, and never presented as cross-platform
//! (acceptance 31.26, 31.31; spec 14.2, 14.10, 14.12, 26.2, 27.4; roadmap M6
//! exit criterion "Windows-only legacy plugins are labelled as such and never
//! advertised as portable").
//!
//! # What is pinned here, and against which seam
//!
//! Acceptance 31.31 is a statement about a *verdict*, so every test below
//! drives the type that produces the verdict rather than a proxy for it:
//!
//! * The classification engine is [`LegacyDiagnostics`], fed from the published
//!   `compatibility/api-matrix/matrix.toml` through
//!   [`CompatibilityMatrix::classify`]. Nothing here hand-feeds an
//!   [`ApiSupport`] the matrix did not produce: the point of the acceptance
//!   criterion is that the verdict follows from the committed classification of
//!   the APIs a plugin uses, so a test that supplied its own support level would
//!   pin only the plumbing.
//! * The published data is the real `matrix.toml` and `corpus.toml`, loaded from
//!   the workspace, not a synthetic fixture. A synthetic matrix cannot catch a
//!   Win32 row that drifts into a portable module, which is the failure
//!   acceptance 31.26 exists to prevent.
//! * The user-visible rendering is [`CompatibilityReport::render`] and the
//!   [`Display`](std::fmt::Display) spelling of [`PluginClassification`] — the
//!   strings `crikey dev compatibility-report` prints.
//!
//! # The two library contracts this file pins beyond the plain data checks
//!
//! 1. `PluginClassification::is_portable(self) -> bool`. `ApiSupport` carries a
//!    portability verdict per API; this is the same verdict for the *package*
//!    classification published in `corpus.toml`, without which "never presented
//!    as cross-platform" would be satisfied only by the absence of any
//!    statement. Contract: true only for the classifications that assert the
//!    package runs off Windows — `works-unchanged`,
//!    `works-with-configuration-changes`, `works-with-minimal-source-changes`,
//!    `works-only-under-legacy-optimized` and `requires-legacy-strict`. False
//!    for `windows-only-but-compatible` (acceptance 31.31), for the three
//!    `blocked-*` states (a package that cannot run anywhere is not portable)
//!    and for `untested` (unknown is not a portability claim).
//! 2. `LegacyDiagnostics::observe_declared_classification(&mut self, owner,
//!    PluginClassification) -> Recorded`. [`LegacyDiagnostics::is_portable`]
//!    answers over the findings on file, so without this a package the corpus
//!    already documents as Windows-only would read as portable on any host that
//!    never observed its Win32 access — the state acceptance 31.31 forbids,
//!    reachable by doing nothing. Contract: a declared classification that is
//!    not portable files the `declared-non-portable` finding, a portable one
//!    files nothing (`Recorded::Clean`) so the call cannot invent findings. The
//!    finding is deliberately *not* `windows-only-dependency`: only one of the
//!    non-portable states is about Windows, and the corpus's Windows-only
//!    packages reach Win32 through their own COM and `ctypes` code rather than
//!    through `keypirinha_wintypes`, so that code would name a §14.12
//!    dependency none of them has.
//!
//! # Determinism
//!
//! No clock, no network, no temporary files, no ambient environment: every input
//! is either a committed data file or a literal in this file, and every verdict
//! is host-independent. A Win32-backed API is classified Windows-only on Windows
//! too — only whether its symbols resolve depends on the host — so nothing here
//! is behind `cfg!(windows)`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crikey_core::PluginId;
use crikey_legacy_compat::{
    ApiSupport, CompatibilityMatrix, CompatibilityReport, LegacyDiagnostics, PluginClassification,
    PluginCorpus, Recorded, Severity,
};

// ---------------------------------------------------------------------------
// The committed data
// ---------------------------------------------------------------------------

/// `CARGO_MANIFEST_DIR` is `<root>/crates/crikey-legacy-compat`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR should have a workspace root two levels up")
        .to_path_buf()
}

fn load_matrix() -> CompatibilityMatrix {
    let path = workspace_root().join("compatibility/api-matrix/matrix.toml");
    CompatibilityMatrix::load(&path).unwrap_or_else(|error| panic!("{} must load: {error}", path.display()))
}

fn load_corpus() -> PluginCorpus {
    let path = workspace_root().join("compatibility/real-plugin-corpus/corpus.toml");
    PluginCorpus::load(&path).unwrap_or_else(|error| panic!("{} must load: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// The explicit platform interface (acceptance 31.26)
// ---------------------------------------------------------------------------

/// The one module through which Win32 is reachable (spec 14.2). Acceptance
/// 31.26 is exactly the claim that this list has one element.
const PLATFORM_INTERFACE_MODULE: &str = "keypirinha_wintypes";

/// The documented legacy modules that carry no platform-specific service and
/// must therefore never classify a Win32 entry point.
const CROSS_PLATFORM_MODULES: &[&str] = &["keypirinha", "keypirinha_util", "keypirinha_net"];

/// The Win32 entry points named by the shim's `WINDOWS_ONLY_SYMBOLS`
/// (`python/keypirinha_wintypes.py`). Duplicated here on purpose: the shim
/// enumerates them so a report can name them without importing Windows
/// machinery, and trimming that tuple must be a visible contract change rather
/// than a quiet way to shrink a report.
const WIN32_ENTRY_POINTS: &[&str] = &["kernel32", "user32", "shell32", "ole32", "declare_func", "GUID"];

/// Names in the platform-interface module that resolve everywhere because they
/// *are* the honest-unavailability surface, not Win32 calls.
const PLATFORM_INTERFACE_PORTABLE_SURFACE: &[&str] = &[
    "WINDOWS_ONLY",
    "WINDOWS_ONLY_SYMBOLS",
    "WindowsOnlyError",
    "is_available",
];

// ---------------------------------------------------------------------------
// Plugin fixtures: identical but for the API they use
// ---------------------------------------------------------------------------

/// The API surface both fixtures below use, and nothing else.
///
/// Deliberately not all `full`: `keypirinha_util.set_clipboard` is `partial`, so
/// the portable fixture carries a real finding. A verdict of "portable" that is
/// merely "no findings at all" would pass a weaker fixture and is the bug this
/// composition kills.
const SHARED_APIS: &[(&str, &str)] = &[
    ("keypirinha", "Plugin.on_start"),
    ("keypirinha", "Plugin.set_catalog"),
    ("keypirinha", "CatalogItem.label"),
    ("keypirinha_util", "set_clipboard"),
];

/// The single API that distinguishes the portable fixture. Cross-platform.
const PORTABLE_DISTINGUISHING_API: (&str, &str) = ("keypirinha", "Plugin.set_suggestions");

/// The single API that distinguishes the Windows-only fixture. Win32-backed,
/// reached through the explicit platform interface.
const WINDOWS_DISTINGUISHING_API: (&str, &str) = (PLATFORM_INTERFACE_MODULE, "kernel32");

const WINDOWS_ONLY_CODE: &str = "windows-only-dependency";

/// The code a declared non-portable classification files. Separate from
/// [`WINDOWS_ONLY_CODE`] because the two carry different evidence.
const DECLARED_NON_PORTABLE_CODE: &str = "declared-non-portable";

/// Every corpus package documented as Windows-only, pinned here rather than
/// read out of the file under test.
///
/// A test that derives its expectation from the same data it renders moves with
/// that data: reclassifying `armotic.keypirinha-audioswitcher` as
/// `works-unchanged` would keep every count consistent and every assertion
/// satisfied, which is precisely the regression acceptance 31.31 exists to
/// catch. Adding a Windows-only package to the corpus is a deliberate change
/// and belongs in this list; silently losing one must fail.
const WINDOWS_ONLY_PACKAGES: &[&str] = &["armotic.keypirinha-audioswitcher", "drorharari.keypirinha-svc"];

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

/// Drive the real classification engine over `apis`, taking every support level
/// from the published matrix.
fn classify_plugin(
    matrix: &CompatibilityMatrix,
    owner: &PluginId,
    apis: &[(&str, &str)],
) -> LegacyDiagnostics {
    let mut diagnostics = LegacyDiagnostics::new();
    for (module, symbol) in apis {
        diagnostics.observe_api_access(owner, module, symbol, matrix.classify(module, symbol));
    }
    diagnostics
}

/// The finding codes filed against `owner`, sorted and deduplicated.
fn codes(diagnostics: &LegacyDiagnostics, owner: &PluginId) -> BTreeSet<&'static str> {
    diagnostics
        .warnings_for(owner)
        .iter()
        .map(|record| record.warning.code())
        .collect()
}

/// `SHARED_APIS` plus one distinguishing entry.
fn apis_with(extra: (&'static str, &'static str)) -> Vec<(&'static str, &'static str)> {
    let mut apis = SHARED_APIS.to_vec();
    apis.push(extra);
    apis
}

// ---------------------------------------------------------------------------
// Acceptance 31.31: the verdict follows from the APIs used
// ---------------------------------------------------------------------------

/// A plugin that reaches a Win32 entry point is labelled Windows-only and its
/// portability verdict is `false`.
///
/// Kills the bug where a Win32 access is recorded as some generic "missing API"
/// finding — actionable-looking, platform-neutral, and leaving the plugin
/// advertised as cross-platform.
#[test]
fn a_legacy_plugin_that_uses_a_windows_only_api_is_labelled_windows_only_and_reports_not_portable() {
    let matrix = load_matrix();
    let owner = plugin("legacy.example.needs-win32");

    let (module, symbol) = WINDOWS_DISTINGUISHING_API;
    assert_eq!(
        matrix.classify(module, symbol),
        Some(ApiSupport::WindowsOnly),
        "the published matrix must classify the Win32 entry point `{module}.{symbol}` as \
         windows-only; the verdict under test is derived from this classification"
    );

    let diagnostics = classify_plugin(&matrix, &owner, &apis_with(WINDOWS_DISTINGUISHING_API));

    assert!(
        codes(&diagnostics, &owner).contains(WINDOWS_ONLY_CODE),
        "a plugin touching `{module}.{symbol}` must be labelled with the `{WINDOWS_ONLY_CODE}` \
         finding, not a platform-neutral one; found {:?}",
        codes(&diagnostics, &owner)
    );
    assert!(
        !diagnostics.is_portable(&owner),
        "acceptance 31.31: a plugin that needs Win32 must never be presented as cross-platform"
    );

    let record = diagnostics
        .warnings_for(&owner)
        .iter()
        .find(|record| record.warning.code() == WINDOWS_ONLY_CODE)
        .expect("the windows-only finding was asserted present above");
    let message = record.warning.message();
    assert!(
        message.contains(module) && message.contains(symbol),
        "the finding must name the module and the entry point the developer has to guard; got {message:?}"
    );
}

/// A plugin whose whole API surface is cross-platform is not labelled
/// Windows-only, and still reports portable even though it carries a finding.
///
/// Kills two bugs at once: labelling every plugin that imports anything from the
/// legacy layer as Windows-only, and computing portability as "has no findings",
/// which would make the `partial` clipboard API look like a platform limitation.
#[test]
fn a_legacy_plugin_that_uses_only_cross_platform_apis_is_not_labelled_windows_only() {
    let matrix = load_matrix();
    let owner = plugin("legacy.example.portable");

    let apis = apis_with(PORTABLE_DISTINGUISHING_API);
    for (module, symbol) in &apis {
        let support = matrix
            .classify(module, symbol)
            .unwrap_or_else(|| panic!("`{module}.{symbol}` must be classified in the published matrix"));
        assert!(
            support.is_portable(),
            "fixture drift: `{module}.{symbol}` is classified `{support}`, which is not a \
             cross-platform classification"
        );
    }

    let diagnostics = classify_plugin(&matrix, &owner, &apis);

    assert!(
        !codes(&diagnostics, &owner).contains(WINDOWS_ONLY_CODE),
        "a plugin using only cross-platform APIs must never be labelled Windows-only; found {:?}",
        codes(&diagnostics, &owner)
    );
    assert!(
        !diagnostics.warnings_for(&owner).is_empty(),
        "the fixture must carry at least one finding — from the `partial` clipboard API — so that \
         the portable verdict below cannot be satisfied by an empty store"
    );
    assert!(
        diagnostics.is_portable(&owner),
        "a documented gap that is not a platform dependency must not cost a plugin its portability"
    );
}

/// The verdict is a function of the APIs used, not of the plugin's identity.
///
/// The two fixtures differ in exactly one tuple and agree in everything else,
/// including the code path that classifies them; the identities are then swapped
/// and the verdicts must travel with the API sets, not with the names. Kills the
/// implementation that hardcodes a list of known-Windows package ids — which
/// passes every single-fixture test and is wrong for every plugin not on the
/// list.
#[test]
fn the_windows_only_label_follows_the_apis_a_plugin_uses_and_not_its_package_name() {
    let matrix = load_matrix();
    let portable_apis = apis_with(PORTABLE_DISTINGUISHING_API);
    let windows_apis = apis_with(WINDOWS_DISTINGUISHING_API);

    assert_eq!(
        portable_apis.len(),
        windows_apis.len(),
        "the fixtures must differ in exactly one API for the comparison below to mean anything"
    );
    assert_eq!(
        portable_apis[..SHARED_APIS.len()],
        windows_apis[..SHARED_APIS.len()],
        "the fixtures must be identical apart from their final API"
    );
    assert_ne!(
        portable_apis.last(),
        windows_apis.last(),
        "the fixtures must actually differ in that final API"
    );

    let alpha = plugin("legacy.example.alpha");
    let beta = plugin("legacy.example.beta");

    // Same identity, different API sets: the answers must diverge.
    let alpha_portable = classify_plugin(&matrix, &alpha, &portable_apis);
    let alpha_windows = classify_plugin(&matrix, &alpha, &windows_apis);
    assert!(
        alpha_portable.is_portable(&alpha),
        "the cross-platform API set must yield a portable verdict for `{}`",
        alpha.0
    );
    assert!(
        !alpha_windows.is_portable(&alpha),
        "adding one Windows-only API to the identical plugin must change the verdict for `{}`",
        alpha.0
    );

    // Same API sets, swapped identities: the answers must not move.
    let beta_windows = classify_plugin(&matrix, &beta, &windows_apis);
    let beta_portable = classify_plugin(&matrix, &beta, &portable_apis);
    assert!(
        !beta_windows.is_portable(&beta),
        "the Windows-only API set must yield the same verdict under a different package name"
    );
    assert!(
        beta_portable.is_portable(&beta),
        "the cross-platform API set must yield the same verdict under a different package name"
    );
    assert_eq!(
        codes(&alpha_windows, &alpha),
        codes(&beta_windows, &beta),
        "two plugins using the same APIs must be labelled identically regardless of their ids"
    );
}

/// Every `windows-only` row of the published matrix is reported as unavailable
/// rather than available, on this non-Windows host and on any other.
///
/// `Recorded::Clean` is precisely the engine's "available, nothing to report"
/// answer, so this is the assertion that no Win32 row can be waved through.
/// Iterating the committed file rather than a literal means a future
/// `windows-only` row is covered the moment it is added.
#[test]
fn every_windows_only_row_of_the_published_matrix_is_reported_unavailable_rather_than_available() {
    let matrix = load_matrix();
    let windows_rows = matrix.windows_only_entries();
    assert!(
        !windows_rows.is_empty(),
        "acceptance 31.31 is vacuous unless the matrix classifies at least one Windows-only API"
    );

    for entry in windows_rows {
        assert!(
            !entry.status.is_portable(),
            "`{}.{}` is classified `{}` and must never count as a cross-platform API",
            entry.module,
            entry.symbol,
            entry.status
        );

        // A catch-all row stands for the module's real Win32 entry points, so it
        // is exercised through them rather than through the literal `*`.
        let symbols: Vec<&str> = if entry.symbol == "*" {
            WIN32_ENTRY_POINTS.to_vec()
        } else {
            vec![entry.symbol.as_str()]
        };

        for symbol in symbols {
            assert_eq!(
                matrix.classify(&entry.module, symbol),
                Some(ApiSupport::WindowsOnly),
                "`{}.{symbol}` must classify as windows-only, through the exact row or the \
                 module's catch-all",
                entry.module
            );

            let owner = plugin("legacy.example.matrix-row");
            let mut diagnostics = LegacyDiagnostics::new();
            let recorded = diagnostics.observe_api_access(
                &owner,
                &entry.module,
                symbol,
                matrix.classify(&entry.module, symbol),
            );
            assert_ne!(
                recorded,
                Recorded::Clean,
                "`{}.{symbol}` is Win32-backed: reporting it as available on a host without Win32 \
                 is the dishonest green tick acceptance 31.31 forbids",
                entry.module
            );
            assert!(
                codes(&diagnostics, &owner).contains(WINDOWS_ONLY_CODE),
                "`{}.{symbol}` must be reported as a Windows-only dependency; found {:?}",
                entry.module,
                codes(&diagnostics, &owner)
            );
            assert!(
                !diagnostics.is_portable(&owner),
                "no Windows-only row may leave the plugin that touches it advertised as portable"
            );
        }
    }
}

/// A Windows-only label survives every later cross-platform observation.
///
/// Kills the "latest observation wins" and "recompute from the last finding"
/// implementations: a plugin that guards its Win32 branch and then does a great
/// deal of portable work is still not portable.
#[test]
fn a_windows_only_label_cannot_be_laundered_away_by_subsequent_cross_platform_use() {
    let matrix = load_matrix();
    let owner = plugin("legacy.example.launders");
    let mut diagnostics = LegacyDiagnostics::new();

    let (module, symbol) = WINDOWS_DISTINGUISHING_API;
    diagnostics.observe_api_access(&owner, module, symbol, matrix.classify(module, symbol));
    assert!(
        !diagnostics.is_portable(&owner),
        "precondition: the plugin starts non-portable"
    );

    for _ in 0..8 {
        for (module, symbol) in SHARED_APIS {
            diagnostics.observe_api_access(&owner, module, symbol, matrix.classify(module, symbol));
        }
        assert!(
            !diagnostics.is_portable(&owner),
            "portable API use must never overwrite a recorded Windows-only dependency"
        );
    }

    assert!(
        codes(&diagnostics, &owner).contains(WINDOWS_ONLY_CODE),
        "the Windows-only finding must still be present after {} portable observations",
        8 * SHARED_APIS.len()
    );
}

// ---------------------------------------------------------------------------
// Acceptance 31.26: platform services only through an explicit interface
// ---------------------------------------------------------------------------

/// Every Win32-backed API in the published matrix lives in the one module
/// declared as the platform interface.
///
/// Kills the drift where a Win32-backed symbol is classified `windows-only`
/// inside `keypirinha` or `keypirinha_util`: the classification would still be
/// honest, but the platform service would no longer be reached through an
/// explicit interface, and a plugin author reading the portable modules would
/// have no way to know one of their calls is Windows-only.
#[test]
fn every_windows_only_api_is_confined_to_the_explicitly_declared_platform_interface_module() {
    let matrix = load_matrix();

    for entry in matrix.windows_only_entries() {
        assert_eq!(
            entry.module, PLATFORM_INTERFACE_MODULE,
            "acceptance 31.26: the Win32-backed `{}.{}` must be reached through the explicit \
             `{PLATFORM_INTERFACE_MODULE}` interface, not through a module documented as \
             cross-platform",
            entry.module, entry.symbol
        );
    }
}

/// No Win32 entry point is reachable through a module documented as portable.
///
/// The complement of the test above, driven from the entry-point names rather
/// than from the matrix rows: it also fails if a portable module grows a
/// catch-all row that starts answering for `kernel32` and friends.
#[test]
fn no_win32_entry_point_is_reachable_through_a_module_documented_as_cross_platform() {
    let matrix = load_matrix();

    for module in CROSS_PLATFORM_MODULES {
        for entry_point in WIN32_ENTRY_POINTS {
            assert_eq!(
                matrix.classify(module, entry_point),
                None,
                "`{module}.{entry_point}` must not resolve to any classification: Win32 entry \
                 points are reachable only through `{PLATFORM_INTERFACE_MODULE}` (acceptance 31.26)"
            );
        }
        for entry_point in WIN32_ENTRY_POINTS {
            assert_eq!(
                matrix.classify(PLATFORM_INTERFACE_MODULE, entry_point),
                Some(ApiSupport::WindowsOnly),
                "the same entry point must be classified windows-only through the explicit \
                 interface, or the assertion above passes for the wrong reason"
            );
        }
    }
}

/// The platform interface exposes exactly one cross-platform surface: the four
/// names that let a plugin ask whether Win32 is there.
///
/// Kills the "helpful stub" regression — classifying a Win32 entry point as
/// `full` inside the platform module so a report goes green — and the opposite
/// one, classifying the availability probe itself as `windows-only`, which would
/// make the guard a portable plugin is told to use cost it its portability.
#[test]
fn the_platform_interface_module_classifies_only_its_availability_surface_as_cross_platform() {
    let matrix = load_matrix();

    let portable_rows: BTreeSet<&str> = matrix
        .portable_entries()
        .into_iter()
        .filter(|entry| entry.module == PLATFORM_INTERFACE_MODULE)
        .map(|entry| entry.symbol.as_str())
        .collect();
    let expected: BTreeSet<&str> = PLATFORM_INTERFACE_PORTABLE_SURFACE.iter().copied().collect();

    assert_eq!(
        portable_rows, expected,
        "only the honest-unavailability surface of `{PLATFORM_INTERFACE_MODULE}` may be classified \
         as cross-platform; everything else in it is a Win32 entry point"
    );

    assert_eq!(
        matrix.classify(PLATFORM_INTERFACE_MODULE, "*"),
        Some(ApiSupport::WindowsOnly),
        "the module's catch-all row must be windows-only so an entry point nobody enumerated is \
         still refused rather than unclassified"
    );
}

// ---------------------------------------------------------------------------
// Acceptance 31.31: the published corpus and the report a user reads
// ---------------------------------------------------------------------------

/// Every package classification carries an explicit portability verdict, and
/// `windows-only-but-compatible` is never one of the portable ones.
///
/// RED: `PluginClassification::is_portable` does not exist yet (see the module
/// header). The per-variant table means an implementation that answers `true`
/// for `untested` — "not yet audited" quietly reading as "runs anywhere" — fails
/// just as loudly as one that answers `true` for the Windows-only state.
#[test]
fn every_package_classification_states_whether_it_may_be_presented_as_cross_platform() {
    const EXPECTED: &[(PluginClassification, bool)] = &[
        (PluginClassification::WorksUnchanged, true),
        (PluginClassification::WorksWithConfigurationChanges, true),
        (PluginClassification::WorksWithMinimalSourceChanges, true),
        (PluginClassification::WindowsOnlyButCompatible, false),
        (PluginClassification::BlockedMissingApis, false),
        (PluginClassification::BlockedPythonVersion, false),
        (PluginClassification::BlockedUndocumentedBehaviour, false),
        (PluginClassification::WorksOnlyUnderLegacyOptimized, true),
        (PluginClassification::RequiresLegacyStrict, true),
        (PluginClassification::Untested, false),
    ];

    assert_eq!(
        EXPECTED.len(),
        PluginClassification::ALL.len(),
        "every classification must state a portability verdict; a new variant needs a row here"
    );

    for (classification, expected) in EXPECTED {
        assert_eq!(
            classification.is_portable(),
            *expected,
            "`{classification}` must{} be presentable as cross-platform (acceptance 31.31)",
            if *expected { "" } else { " not" }
        );
    }
}

/// The published corpus contains real Windows-only packages, and not one of them
/// reads as portable — through the verdict or through the string a user sees.
///
/// The expected package ids are pinned in [`WINDOWS_ONLY_PACKAGES`] rather than
/// filtered out of the corpus, so a package quietly reclassified out of the
/// Windows-only state fails here instead of shrinking the set this test
/// iterates. The classification-string assertion is what stops a package being
/// described in wording that omits the limitation.
#[test]
fn no_windows_only_package_in_the_published_corpus_reads_as_portable() {
    let corpus = load_corpus();

    let windows_only: Vec<&_> = corpus
        .entries()
        .iter()
        .filter(|entry| entry.classification == PluginClassification::WindowsOnlyButCompatible)
        .collect();
    let found: Vec<&str> = windows_only.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(
        found, WINDOWS_ONLY_PACKAGES,
        "acceptance 31.31 requires the project to be able to say a real package is Windows-only; \
         the corpus must classify exactly the pinned packages that way"
    );

    for entry in windows_only {
        assert!(
            !entry.classification.is_portable(),
            "the corpus package `{}` depends on Windows and must never be presented as \
             cross-platform",
            entry.id
        );
        let rendered = entry.classification.to_string();
        assert!(
            rendered.contains("windows-only"),
            "the classification a user reads for `{}` must name the limitation; got {rendered:?}",
            entry.id
        );
        for candidate in PluginClassification::ALL {
            if candidate.is_portable() {
                assert_ne!(
                    rendered,
                    candidate.to_string(),
                    "the rendered classification for `{}` must not be a spelling that also names a \
                     portable state",
                    entry.id
                );
            }
        }
    }
}

/// The rendered compatibility report attributes Windows-only packages to their
/// own bucket, and its portability totals never count one of them as portable.
///
/// This is the report a user sees: `crikey dev compatibility-report` prints
/// `render()` byte for byte. The Windows-only bucket is compared against
/// [`WINDOWS_ONLY_PACKAGES`], not against a count folded out of the same corpus
/// the report renders: an expectation derived from the data under test moves
/// with it, so reclassifying a Windows-only package would satisfy both sides at
/// once. Everything else asserted here is an internal-consistency claim the
/// pinned count anchors.
#[test]
fn the_rendered_compatibility_report_never_files_a_windows_only_package_under_a_portable_heading() {
    let matrix = load_matrix();
    let corpus = load_corpus();
    let rendered = CompatibilityReport::new(&matrix, &corpus).render();

    let count = |key: &str| -> usize {
        let prefix = format!("{key}=");
        let line = rendered
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("the report must carry a `{key}` line; got:\n{rendered}"));
        line[prefix.len()..]
            .parse()
            .unwrap_or_else(|error| panic!("`{line}` must carry a count: {error}"))
    };

    let windows_only = WINDOWS_ONLY_PACKAGES.len();
    assert_eq!(
        count("corpus_windows_only_but_compatible"),
        windows_only,
        "the rendered report must count the {windows_only} pinned Windows-only package(s) in \
         their own bucket; got:\n{rendered}"
    );

    let portable_buckets: usize = PluginClassification::ALL
        .into_iter()
        .filter(|classification| classification.is_portable())
        .map(|classification| count(&format!("corpus_{}", classification.slug().replace('-', "_"))))
        .sum();
    assert_eq!(
        count("corpus_portable"),
        portable_buckets,
        "the report's portable total must hold exactly the packages in buckets whose \
         classification permits a cross-platform claim; got:\n{rendered}"
    );
    assert_eq!(
        count("corpus_portable") + count("corpus_not_portable"),
        count("corpus_plugins"),
        "every referenced package must land on one side of the portability question; got:\n\
         {rendered}"
    );
    assert!(
        count("corpus_not_portable") >= windows_only,
        "at least the {windows_only} pinned Windows-only package(s) must fall outside the \
         portable total; got:\n{rendered}"
    );
}

/// A package the corpus already documents as Windows-only cannot be reported
/// portable merely because this host never observed its Win32 access.
///
/// The first assertion records the hole this entry point closes — an unobserved
/// plugin reads as portable — and the rest pin the refusal. The
/// `works-unchanged` case is asserted `Clean` so the entry point cannot
/// manufacture findings for portable packages.
#[test]
fn a_package_declared_windows_only_is_refused_a_portable_verdict_even_with_no_observed_win32_access() {
    let declared = plugin("legacy.example.declared-windows-only");
    let mut diagnostics = LegacyDiagnostics::new();

    assert!(
        diagnostics.warnings_for(&declared).is_empty(),
        "precondition: nothing has been observed about this plugin, which is exactly the state in \
         which a Windows-only package must not be able to pass as portable"
    );

    let recorded = diagnostics
        .observe_declared_classification(&declared, PluginClassification::WindowsOnlyButCompatible);
    assert_eq!(
        recorded,
        Recorded::Retained,
        "declaring a package Windows-only must file a finding, not be silently accepted"
    );
    assert!(
        !diagnostics.is_portable(&declared),
        "acceptance 31.31: a documented Windows-only package must be refused a portable verdict on \
         every host, including one that never ran its Win32 branch"
    );
    assert!(
        codes(&diagnostics, &declared).contains(DECLARED_NON_PORTABLE_CODE),
        "a declared classification must surface under its own stable code; found {:?}",
        codes(&diagnostics, &declared)
    );
    assert!(
        !codes(&diagnostics, &declared).contains(WINDOWS_ONLY_CODE),
        "the corpus's Windows-only packages reach Win32 through their own COM and `ctypes` code, \
         so a declaration is no evidence of a `{PLATFORM_INTERFACE_MODULE}` entry point and must \
         not be filed as one (spec 14.12); found {:?}",
        codes(&diagnostics, &declared)
    );

    let portable = plugin("legacy.example.declared-portable");
    assert_eq!(
        diagnostics.observe_declared_classification(&portable, PluginClassification::WorksUnchanged),
        Recorded::Clean,
        "a classification that permits a cross-platform claim must file nothing: a report that \
         invents findings for healthy packages trains developers to ignore it"
    );
    assert!(
        diagnostics.is_portable(&portable),
        "a package classified `works-unchanged` must keep its portable verdict"
    );
    assert!(
        !diagnostics.is_portable(&declared),
        "one plugin's declared classification must never leak into another's verdict"
    );
}

/// The non-portable states that are not about Windows are refused a portable
/// verdict without any Win32 claim being invented for them.
///
/// `blocked-missing-apis` and `untested` describe a package that CriKey cannot
/// run and a package nobody has run; neither carries evidence of a Win32
/// dependency, and a §14.12 diagnostic naming one would send a developer
/// looking for Windows code that is not in the package. The severities are
/// pinned alongside because the three declarations differ in how bad they are:
/// blocked everywhere is a blocking finding, never audited is an observation.
#[test]
fn a_non_windows_non_portable_declaration_is_never_reported_as_a_win32_dependency() {
    for (classification, expected) in [
        (PluginClassification::BlockedMissingApis, Severity::Blocking),
        (PluginClassification::BlockedPythonVersion, Severity::Blocking),
        (
            PluginClassification::BlockedUndocumentedBehaviour,
            Severity::Blocking,
        ),
        (PluginClassification::Untested, Severity::Info),
    ] {
        let owner = plugin(&format!("legacy.example.{}", classification.slug()));
        let mut diagnostics = LegacyDiagnostics::new();
        assert_eq!(
            diagnostics.observe_declared_classification(&owner, classification),
            Recorded::Retained,
            "`{classification}` does not permit a cross-platform claim, so it must file a finding"
        );
        assert!(
            !diagnostics.is_portable(&owner),
            "`{classification}` must be refused a portable verdict (acceptance 31.31)"
        );
        assert_eq!(
            codes(&diagnostics, &owner),
            BTreeSet::from([DECLARED_NON_PORTABLE_CODE]),
            "`{classification}` is no evidence of a Win32 dependency and must file only the \
             declared-classification finding"
        );

        let record = &diagnostics.warnings_for(&owner)[0];
        assert_eq!(
            record.warning.severity(),
            expected,
            "`{classification}` must be reported at the weight its own state deserves"
        );
        let message = record.warning.message();
        assert!(
            message.contains(classification.slug()),
            "the message must name the classification it came from; got {message:?}"
        );
        assert!(
            !message.contains(PLATFORM_INTERFACE_MODULE),
            "the message must not claim a `{PLATFORM_INTERFACE_MODULE}` dependency the package \
             does not have; got {message:?}"
        );
    }
}
