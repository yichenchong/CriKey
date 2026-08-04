//! Legacy compatibility diagnostics for M3 (spec 26.1, 26.2, 14.12, 14.2, 9.2,
//! 9.6, 25.2; roadmap M3; acceptance 31.29, 31.31).
//!
//! Spec 26.2 lists nine things CriKey should report about a legacy plugin. Eight
//! of them are *categories* of finding; the ninth — "suggested source changes" —
//! is not a category at all but an obligation attached to the others. That
//! reading is what this file pins: [`CompatibilityWarning::code`] enumerates the
//! eight, and [`CompatibilityWarning::suggestion`] is the ninth, present on
//! exactly the findings a plugin author can act on in source.
//!
//! # Why a warning is a value, not a log line
//!
//! Acceptance 31.29 asks for *actionable* diagnostics. A formatted string is not
//! actionable: it cannot be counted, deduplicated, bounded, filtered by plugin,
//! or turned into a compatibility-matrix entry. So every finding here is a typed
//! [`CompatibilityIssue`] carrying the subject it concerns, wrapped in a
//! [`CompatibilityWarning`] carrying the plugin it belongs to, and the human
//! prose is *derived* from that value rather than being the value.
//!
//! Three things follow, and each is a test below:
//!
//! * The `code` is the machine's handle on the finding and never changes; the
//!   `message` is the human's and may. A test that asserts on prose would freeze
//!   the wrong half, so the tests assert prose only names its subject.
//! * Deduplication keys on `(plugin, code)`. A plugin in a failure loop reports
//!   the same finding thousands of times, and a diagnostics store that grows
//!   with it is a denial-of-service surface reachable from plugin code.
//! * The store is bounded per plugin, and dropping is itself reported. Silently
//!   discarding findings would make an empty report indistinguishable from a
//!   clean one, which is exactly the "plausible lie" the roadmap forbids.
//!
//! # Why there is no interpreter, no worker and no clock here
//!
//! Diagnostics are a pure fold over observations. The observations —
//! "`keypirinha_wintypes.kernel32` raised `WindowsOnlyError` on this host",
//! "`on_suggest` ran 1,800 ms and never read `should_terminate()`" — are made by
//! the worker and the runtime, which own their own slices and their own tests.
//! This file feeds those observations in as values, so every assertion is
//! deterministic on every platform and nothing sleeps, spawns or samples a
//! clock.
//!
//! That includes the Windows-only case. Constraint: on this Linux host
//! `keypirinha_wintypes` *imports successfully* and every Win32 entry point
//! raises a typed error (spec 14.2, 14.12) — so the honest observation is
//! [`ImportOutcome::WindowsOnly`], never [`ImportOutcome::Missing`], and the
//! detail string below is the one the shim actually produces. The mapping from
//! that observation to a `windows-only-dependency` warning and to
//! `is_portable() == false` (acceptance 31.31) is platform-independent, so it is
//! asserted unconditionally rather than hidden behind `#[cfg(windows)]`.

use crikey_core::PluginId;
use crikey_input_scheduler::SchedulingProfile;
use crikey_legacy_compat::{
    ApiSupport, CallbackObservation, CompatibilityIssue, CompatibilityWarning, DiagnosticLimits,
    ImportOutcome, LegacyCallback, LegacyDiagnostics, PythonVersion, Recorded, Severity, WarningRecord,
    MINIMUM_SUPPORTED_PYTHON,
};

// ---------------------------------------------------------------------------
// The contract's fixed vocabulary
// ---------------------------------------------------------------------------

/// The eight reportable categories of spec 26.2, by stable code.
///
/// The ninth item of that list, "suggested source changes", is not a category:
/// it is [`CompatibilityWarning::suggestion`], asserted per category below.
const DOCUMENTED_CATEGORIES: [&str; 8] = [
    // "Missing API calls."
    "missing-api",
    // "Unsupported imports."
    "unsupported-import",
    // "Python-version incompatibilities."
    "python-version-incompatible",
    // "Windows-only dependencies."
    "windows-only-dependency",
    // "Native extension requirements."
    "native-extension-required",
    // "Undocumented API access where detectable." (spec 14.12)
    "undocumented-api-access",
    // "Scheduling profile."
    "scheduling-profile",
    // "Long callbacks that do not check `should_terminate()`."
    "long-callback-without-termination-check",
];

/// The one code that is *not* a spec 26.2 finding: the store reporting on
/// itself when a plugin has exhausted its share of the store.
const OVERFLOW_CODE: &str = "diagnostics-overflow";

/// The interpreter this host offers (`python3` is CPython 3.14.4 here), used as
/// the "available" side of every version comparison.
const HOST_PYTHON: PythonVersion = PythonVersion::new(3, 14, 4);

/// What the Linux `keypirinha_wintypes` shim raises when a Win32 entry point is
/// touched. Reproduced verbatim so the retained detail is the thing a developer
/// would have seen in the plugin's stderr, not a paraphrase of it.
const WINTYPES_ERROR: &str = "WindowsOnlyError: keypirinha_wintypes.kernel32 is a Windows-only Win32 \
                              entry point and is unavailable on platform 'linux'";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

fn diagnostics() -> LegacyDiagnostics {
    LegacyDiagnostics::new()
}

/// One instance of every category of spec 26.2, paired with the code it must
/// carry, a substring its human-readable message must name, and whether spec
/// 26.2's "suggested source changes" applies to it.
///
/// A suggestion is owed wherever a plugin author could change source to resolve
/// the finding. It is *not* owed for the scheduling profile, which is a fact
/// about how CriKey will run the plugin and not a defect to fix.
fn documented_categories() -> Vec<(CompatibilityIssue, &'static str, &'static str, bool)> {
    vec![
        (
            CompatibilityIssue::MissingApi {
                module: "keypirinha".to_owned(),
                symbol: "Plugin.set_actions".to_owned(),
                support: ApiSupport::Planned,
            },
            "missing-api",
            "Plugin.set_actions",
            true,
        ),
        (
            CompatibilityIssue::UnsupportedImport {
                module: "win32com.client".to_owned(),
                detail: "ModuleNotFoundError: No module named 'win32com'".to_owned(),
            },
            "unsupported-import",
            "win32com.client",
            true,
        ),
        (
            CompatibilityIssue::PythonVersionIncompatible {
                required: PythonVersion::new(4, 0, 0),
                available: HOST_PYTHON,
            },
            "python-version-incompatible",
            "4.0.0",
            true,
        ),
        (
            CompatibilityIssue::WindowsOnlyDependency {
                module: "keypirinha_wintypes".to_owned(),
                entry_point: "kernel32".to_owned(),
                detail: WINTYPES_ERROR.to_owned(),
            },
            "windows-only-dependency",
            "keypirinha_wintypes",
            true,
        ),
        (
            CompatibilityIssue::NativeExtensionRequired {
                module: "lxml.etree".to_owned(),
                artifact: "etree.cp314-win_amd64.pyd".to_owned(),
            },
            "native-extension-required",
            "lxml.etree",
            true,
        ),
        (
            CompatibilityIssue::UndocumentedApiAccess {
                module: "keypirinha".to_owned(),
                symbol: "_registry".to_owned(),
            },
            "undocumented-api-access",
            "_registry",
            true,
        ),
        (
            CompatibilityIssue::SchedulingProfileReported {
                profile: SchedulingProfile::LegacyStrict,
            },
            "scheduling-profile",
            "legacy-strict",
            false,
        ),
        (
            CompatibilityIssue::LongCallbackWithoutTerminationCheck {
                callback: LegacyCallback::OnSuggest,
                duration_ms: 1_800,
                threshold_ms: 500,
            },
            "long-callback-without-termination-check",
            "on_suggest",
            true,
        ),
    ]
}

/// The single record a plugin holds for `code`, or a failure naming everything
/// it holds instead — the two ways this lookup goes wrong (nothing, or more than
/// one) are both contract violations worth naming precisely.
fn record_for<'a>(diagnostics: &'a LegacyDiagnostics, owner: &PluginId, code: &str) -> &'a WarningRecord {
    let held = codes(diagnostics, owner);
    let mut matching = diagnostics
        .warnings_for(owner)
        .iter()
        .filter(|record| record.warning.code() == code);
    let found = matching.next().unwrap_or_else(|| {
        panic!(
            "`{owner}` holds no `{code}` warning; it holds {held:?}",
            owner = owner.0
        )
    });
    assert!(
        matching.next().is_none(),
        "`{owner}` holds `{code}` more than once, so deduplication is not keyed on the code; it \
         holds {held:?}",
        owner = owner.0
    );
    found
}

fn codes(diagnostics: &LegacyDiagnostics, owner: &PluginId) -> Vec<&'static str> {
    diagnostics
        .warnings_for(owner)
        .iter()
        .map(|record| record.warning.code())
        .collect()
}

/// A missing-module observation, the shape the worker reports for an import the
/// interpreter could not resolve at all.
fn missing(detail: &str) -> ImportOutcome {
    ImportOutcome::Missing {
        detail: detail.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Spec 26.2: every category is reportable, and says who, what and how to fix it
// ---------------------------------------------------------------------------

#[test]
fn every_documented_category_names_its_plugin_a_stable_code_and_readable_prose() {
    let owner = plugin("legacy.example.everything");
    let mut diagnostics = diagnostics();
    let categories = documented_categories();

    assert_eq!(
        categories.len(),
        DOCUMENTED_CATEGORIES.len(),
        "spec 26.2 lists {} reportable categories; this fixture covers {}",
        DOCUMENTED_CATEGORIES.len(),
        categories.len(),
    );

    for (issue, expected_code, subject, owes_suggestion) in categories {
        let warning = CompatibilityWarning {
            plugin: owner.clone(),
            issue: issue.clone(),
        };

        assert_eq!(
            warning.plugin, owner,
            "a warning that does not carry its plugin cannot be routed to the developer who can \
             act on it: {issue:?}"
        );

        let code = warning.code();
        assert_eq!(code, expected_code, "wrong stable code for {issue:?}");
        assert!(
            DOCUMENTED_CATEGORIES.contains(&code),
            "`{code}` is not one of the spec 26.2 categories {DOCUMENTED_CATEGORIES:?}"
        );
        assert!(
            !code.is_empty()
                && code
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '-'),
            "`{code}` is not a stable lowercase-kebab machine code, so it cannot be grepped or \
             stored as a key"
        );

        let message = warning.message();
        assert!(
            message.contains(subject),
            "the `{code}` message does not name the `{subject}` it concerns: {message:?}"
        );
        assert!(
            message.contains(' ') && message != code,
            "the `{code}` message is a slug, not the human-readable prose spec 26.1 asks for: \
             {message:?}"
        );

        let suggestion = warning.suggestion();
        assert_eq!(
            suggestion.is_some(),
            owes_suggestion,
            "spec 26.2's \"suggested source changes\" obligation is wrong for `{code}`: got \
             {suggestion:?}"
        );
        if let Some(suggestion) = &suggestion {
            assert!(
                suggestion.contains(' '),
                "the `{code}` suggestion is not a source change a developer can follow: \
                 {suggestion:?}"
            );
        }

        // Reporting the scheduling profile is an observation, not a defect. If it
        // carried the same weight as a blocked import, every conforming legacy
        // plugin would look broken and the report would stop being read.
        let severity = warning.severity();
        if code == "scheduling-profile" {
            assert_eq!(
                severity,
                Severity::Info,
                "reporting the scheduling profile must not make a conforming plugin look broken"
            );
        } else {
            assert!(
                severity >= Severity::Warning,
                "`{code}` is a compatibility defect and must outrank Info, got {severity:?}"
            );
        }

        assert_eq!(
            diagnostics.report(warning),
            Recorded::Retained,
            "the first `{code}` for a plugin must be retained"
        );
    }

    let held = codes(&diagnostics, &owner);
    assert_eq!(
        held.len(),
        DOCUMENTED_CATEGORIES.len(),
        "the store kept {held:?}, not one record per spec 26.2 category"
    );
    for category in DOCUMENTED_CATEGORIES {
        assert!(
            held.contains(&category),
            "the store dropped the `{category}` category: {held:?}"
        );
    }
}

#[test]
fn partial_api_diagnostics_do_not_claim_the_api_is_absent() {
    let warning = CompatibilityWarning {
        plugin: plugin("legacy.example.partial"),
        issue: CompatibilityIssue::MissingApi {
            module: "keypirinha_util".to_owned(),
            symbol: "set_clipboard".to_owned(),
            support: ApiSupport::Partial,
        },
    };
    let message = warning.message();
    assert!(
        message.contains("partially available") && message.contains("partial"),
        "a partial API must be described as partial rather than absent: {message}"
    );
    let suggestion = warning.suggestion().expect("partial support is actionable");
    assert!(
        suggestion.contains("partially supported"),
        "a partial API suggestion must address the documented gap, not pretend the symbol is absent: \
         {suggestion}"
    );
}

// ---------------------------------------------------------------------------
// Spec 26.2: unsupported imports
// ---------------------------------------------------------------------------

#[test]
fn an_import_the_layer_cannot_provide_is_reported_by_module_rather_than_crashing() {
    let owner = plugin("legacy.example.importer");
    let mut diagnostics = diagnostics();

    let outcome = missing("ModuleNotFoundError: No module named 'win32com'");
    assert_eq!(
        diagnostics.observe_import(&owner, "win32com.client", outcome),
        Recorded::Retained,
        "an import the layer does not provide is a reportable finding, not a no-op"
    );

    let record = record_for(&diagnostics, &owner, "unsupported-import");
    match &record.warning.issue {
        CompatibilityIssue::UnsupportedImport { module, detail } => {
            assert_eq!(
                module, "win32com.client",
                "the warning must name the module the plugin asked for, or the developer has \
                 nothing to remove"
            );
            assert!(
                detail.contains("ModuleNotFoundError"),
                "the interpreter's own words are the most useful detail there is: {detail:?}"
            );
        }
        other => panic!("an unresolvable import was classified as {other:?}"),
    }
    assert!(
        record.warning.message().contains("win32com.client"),
        "the prose must name the module too: {:?}",
        record.warning.message()
    );
    assert!(
        record.warning.suggestion().is_some(),
        "an unsupported import is exactly the case spec 26.2's suggested source change exists for"
    );

    // "Not a crash" is only meaningful if the store still works afterwards: a
    // collector that poisons itself on the first bad news reports nothing about
    // the rest of the plugin.
    assert_eq!(
        diagnostics.observe_import(&owner, "keypirinha_util", ImportOutcome::Available),
        Recorded::Clean,
        "a module the layer does provide is not a finding"
    );
    assert_eq!(
        diagnostics.total_warnings(),
        1,
        "the clean import must not have been recorded as anything"
    );
}

// ---------------------------------------------------------------------------
// Spec 14.2, 14.12, acceptance 31.31: Windows-only dependencies
// ---------------------------------------------------------------------------

#[test]
fn a_windows_only_dependency_is_reported_and_the_plugin_is_never_advertised_as_portable() {
    let portable = plugin("legacy.example.portable");
    let windows = plugin("legacy.example.wintypes");
    let matrixed = plugin("legacy.example.matrixed");
    let mut diagnostics = diagnostics();

    // The three modules of spec 14.2 that work everywhere.
    for module in ["keypirinha", "keypirinha_util", "keypirinha_net"] {
        for owner in [&portable, &windows] {
            assert_eq!(
                diagnostics.observe_import(owner, module, ImportOutcome::Available),
                Recorded::Clean,
                "`{module}` is provided on every platform and must not be a finding"
            );
        }
    }

    // On this host the shim imports and then refuses per entry point, so the
    // honest observation is WindowsOnly rather than Missing.
    assert_eq!(
        diagnostics.observe_import(
            &windows,
            "keypirinha_wintypes",
            ImportOutcome::WindowsOnly {
                entry_point: "kernel32".to_owned(),
                detail: WINTYPES_ERROR.to_owned(),
            },
        ),
        Recorded::Retained,
    );

    let record = record_for(&diagnostics, &windows, "windows-only-dependency");
    match &record.warning.issue {
        CompatibilityIssue::WindowsOnlyDependency {
            module,
            entry_point,
            detail,
        } => {
            assert_eq!(module, "keypirinha_wintypes");
            assert_eq!(
                entry_point, "kernel32",
                "the entry point is what the developer has to guard, so the warning must name it"
            );
            assert_eq!(
                detail, WINTYPES_ERROR,
                "the typed refusal the shim raised is the evidence; a paraphrase is not"
            );
        }
        other => panic!("a Win32-backed module was classified as {other:?}"),
    }
    assert_ne!(
        record.warning.code(),
        "unsupported-import",
        "the module imported successfully — calling that a missing import would misdirect the \
         developer to install something that is already there"
    );
    assert!(record.warning.severity() >= Severity::Warning);
    assert!(record.warning.suggestion().is_some());

    // The same conclusion has to be reachable from the compatibility matrix, not
    // just from a failed call: a plugin naming a windows-only symbol is
    // windows-only whether or not it got as far as touching it.
    assert_eq!(
        diagnostics.observe_api_access(
            &matrixed,
            "keypirinha_wintypes",
            "declare_func",
            Some(ApiSupport::WindowsOnly),
        ),
        Recorded::Retained,
    );
    assert_eq!(
        record_for(&diagnostics, &matrixed, "windows-only-dependency")
            .warning
            .code(),
        "windows-only-dependency",
    );

    assert!(
        !diagnostics.is_portable(&windows),
        "acceptance 31.31: a plugin that needs Win32 must not be presented as cross-platform"
    );
    assert!(
        !diagnostics.is_portable(&matrixed),
        "a windows-only API in the matrix is a windows-only dependency too"
    );
    assert!(
        diagnostics.is_portable(&portable),
        "a plugin importing only the portable modules of spec 14.2 stays portable"
    );
    assert!(
        diagnostics.warnings_for(&portable).is_empty(),
        "the portable plugin collected {:?}",
        codes(&diagnostics, &portable)
    );
}

// ---------------------------------------------------------------------------
// Spec 14.11, 26.2: Python-version incompatibilities
// ---------------------------------------------------------------------------

#[test]
fn a_python_requirement_outside_the_supported_range_carries_both_versions() {
    let supported = plugin("legacy.example.supported");
    let too_new = plugin("legacy.example.needs-future-python");
    let too_old = plugin("legacy.example.needs-python-2");
    let mut diagnostics = diagnostics();

    assert!(
        MINIMUM_SUPPORTED_PYTHON <= HOST_PYTHON,
        "this host cannot run the layer at all, so nothing below can be trusted"
    );

    assert_eq!(
        diagnostics.observe_python_requirement(&supported, MINIMUM_SUPPORTED_PYTHON, HOST_PYTHON),
        Recorded::Clean,
        "a plugin asking for the documented floor on a newer interpreter is compatible"
    );

    // Newer than the host can offer.
    let required = PythonVersion::new(4, 0, 0);
    assert_eq!(
        diagnostics.observe_python_requirement(&too_new, required, HOST_PYTHON),
        Recorded::Retained,
    );
    let record = record_for(&diagnostics, &too_new, "python-version-incompatible");
    match &record.warning.issue {
        CompatibilityIssue::PythonVersionIncompatible {
            required: carried_required,
            available,
        } => {
            assert_eq!(*carried_required, required);
            assert_eq!(*available, HOST_PYTHON);
        }
        other => panic!("an unmeetable interpreter requirement was classified as {other:?}"),
    }
    let message = record.warning.message();
    assert!(
        message.contains("4.0.0") && message.contains("3.14.4"),
        "a version incompatibility that names only one of the two versions is unactionable: \
         {message:?}"
    );
    assert!(record.warning.suggestion().is_some());

    // Older than the layer supports: a Python 2 plugin is blocked even though the
    // host interpreter is newer than everything it asks for.
    let ancient = PythonVersion::new(2, 7, 18);
    assert!(ancient < MINIMUM_SUPPORTED_PYTHON);
    assert_eq!(
        diagnostics.observe_python_requirement(&too_old, ancient, HOST_PYTHON),
        Recorded::Retained,
        "a requirement below the supported floor is outside the range, not satisfied by a newer \
         interpreter"
    );
    let message = record_for(&diagnostics, &too_old, "python-version-incompatible")
        .warning
        .message();
    assert!(
        message.contains("2.7.18") && message.contains("3.14.4"),
        "both versions must appear here too: {message:?}"
    );
    assert!(
        message.contains(&MINIMUM_SUPPORTED_PYTHON.to_string()),
        "a plugin blocked by the floor has to be told what the floor is: {message:?}"
    );

    assert!(
        diagnostics.warnings_for(&supported).is_empty(),
        "the compatible plugin collected {:?}",
        codes(&diagnostics, &supported)
    );
}

// ---------------------------------------------------------------------------
// Spec 9.2, 9.6, 25.2, 26.2: long callbacks that never cooperate
// ---------------------------------------------------------------------------

#[test]
fn a_long_callback_is_reported_only_when_it_never_reads_should_terminate() {
    let cooperative = plugin("legacy.example.cooperative");
    let brief = plugin("legacy.example.brief");
    let stubborn = plugin("legacy.example.stubborn");
    let mut diagnostics = diagnostics();

    // Spec 25.2's modern hard query deadline is the documented duration a legacy
    // callback is measured against. Spec 9.6 forbids *killing* the worker at that
    // point; it does not forbid saying so.
    let threshold = diagnostics.limits().long_callback_threshold_ms;
    assert_eq!(
        threshold, 500,
        "the documented duration is spec 25.2's 500 ms modern hard query deadline"
    );

    // Duration alone is not a defect. Spec 9.6 explicitly permits a slow legacy
    // callback; what is reportable is a slow one that cannot be asked to stop.
    assert_eq!(
        diagnostics.observe_callback(
            &cooperative,
            LegacyCallback::OnSuggest,
            CallbackObservation {
                duration_ms: 30_000,
                observed_should_terminate: true,
            },
        ),
        Recorded::Clean,
        "a callback that polls `should_terminate()` is cooperating, however long it runs"
    );

    // Neither is failing to check, on its own: a callback that returns promptly
    // never had an obsolescence to notice.
    for duration_ms in [0, threshold - 1, threshold] {
        assert_eq!(
            diagnostics.observe_callback(
                &brief,
                LegacyCallback::OnSuggest,
                CallbackObservation {
                    duration_ms,
                    observed_should_terminate: false,
                },
            ),
            Recorded::Clean,
            "{duration_ms} ms does not exceed the {threshold} ms threshold"
        );
    }

    assert_eq!(
        diagnostics.observe_callback(
            &stubborn,
            LegacyCallback::OnSuggest,
            CallbackObservation {
                duration_ms: 1_800,
                observed_should_terminate: false,
            },
        ),
        Recorded::Retained,
    );

    let record = record_for(&diagnostics, &stubborn, "long-callback-without-termination-check");
    match &record.warning.issue {
        CompatibilityIssue::LongCallbackWithoutTerminationCheck {
            callback,
            duration_ms,
            threshold_ms,
        } => {
            assert_eq!(
                *callback,
                LegacyCallback::OnSuggest,
                "the developer needs to know which callback to fix"
            );
            assert_eq!(
                *duration_ms, 1_800,
                "the measured duration is the evidence, so it is carried rather than rounded away"
            );
            assert_eq!(*threshold_ms, threshold);
        }
        other => panic!("an uncooperative long callback was classified as {other:?}"),
    }

    // The callback is named the way the plugin's own source names it, so the
    // diagnostic can be searched for in the file it is about.
    assert_eq!(LegacyCallback::OnSuggest.as_str(), "on_suggest");

    let message = record.warning.message();
    assert!(
        message.contains(LegacyCallback::OnSuggest.as_str()) && message.contains("1800"),
        "the prose must name the callback and the duration it measured: {message:?}"
    );
    let suggestion = record
        .warning
        .suggestion()
        .expect("spec 9.4 and 26.2 both require telling the author what to change");
    assert!(
        suggestion.contains("should_terminate"),
        "the only useful source change here is to poll the flag: {suggestion:?}"
    );

    for quiet in [&cooperative, &brief] {
        assert!(
            diagnostics.warnings_for(quiet).is_empty(),
            "`{}` collected {:?}",
            quiet.0,
            codes(&diagnostics, quiet)
        );
    }
}

// ---------------------------------------------------------------------------
// Spec 14.12: undocumented internals get their own diagnostic
// ---------------------------------------------------------------------------

#[test]
fn access_to_an_undocumented_internal_is_its_own_diagnostic_not_a_missing_api() {
    let documented = plugin("legacy.example.documented");
    let planned = plugin("legacy.example.planned");
    let prying = plugin("legacy.example.prying");
    let mut diagnostics = diagnostics();

    // Classified `full` or `behavioural-difference` in the compatibility matrix:
    // documented, present, and therefore nothing to report against the plugin.
    for support in [ApiSupport::Full, ApiSupport::BehaviouralDifference] {
        assert_eq!(
            diagnostics.observe_api_access(
                &documented,
                "keypirinha",
                "Plugin.set_suggestions",
                Some(support),
            ),
            Recorded::Clean,
            "a matrix classification of {support:?} means the call works here"
        );
    }

    // In the matrix, but absent here. This is spec 26.2's "missing API calls":
    // documented, classified, and not something the plugin author invented.
    for support in [ApiSupport::Unsupported, ApiSupport::Partial] {
        let owner = plugin(&format!("legacy.example.{support:?}"));
        assert_eq!(
            diagnostics.observe_api_access(&owner, "keypirinha", "Plugin.set_actions", Some(support)),
            Recorded::Retained,
            "a matrix classification of {support:?} means the call will not work"
        );
        assert_eq!(
            record_for(&diagnostics, &owner, "missing-api").warning.code(),
            "missing-api",
        );
    }
    assert_eq!(
        diagnostics.observe_api_access(
            &planned,
            "keypirinha",
            "Plugin.set_actions",
            Some(ApiSupport::Planned),
        ),
        Recorded::Retained,
    );
    let missing_api = record_for(&diagnostics, &planned, "missing-api").clone();
    match &missing_api.warning.issue {
        CompatibilityIssue::MissingApi {
            module,
            symbol,
            support,
        } => {
            assert_eq!(module, "keypirinha");
            assert_eq!(symbol, "Plugin.set_actions");
            assert_eq!(
                *support,
                ApiSupport::Planned,
                "the matrix classification is why this is missing rather than undocumented"
            );
        }
        other => panic!("a matrixed but absent API was classified as {other:?}"),
    }

    // Not in the matrix at all. Spec 14.12 says CriKey need not support this and
    // *shall produce a specific diagnostic* — the whole point is that it is
    // distinguishable from "we have not written that yet".
    assert_eq!(
        diagnostics.observe_api_access(&prying, "keypirinha", "_registry", None),
        Recorded::Retained,
    );
    let undocumented = record_for(&diagnostics, &prying, "undocumented-api-access");
    match &undocumented.warning.issue {
        CompatibilityIssue::UndocumentedApiAccess { module, symbol } => {
            assert_eq!(module, "keypirinha");
            assert_eq!(symbol, "_registry");
        }
        other => panic!("an unmatrixed internal was classified as {other:?}"),
    }
    assert_ne!(
        undocumented.warning.code(),
        missing_api.warning.code(),
        "reaching into an internal and calling an unimplemented API need different answers: one \
         is the plugin's bug and the other is ours"
    );
    assert_eq!(undocumented.warning.code(), "undocumented-api-access");
    assert!(undocumented.warning.severity() >= Severity::Warning);
    assert!(
        undocumented.warning.message().contains("undocumented"),
        "the prose must say what makes this specific: {:?}",
        undocumented.warning.message()
    );
    assert!(
        undocumented.warning.suggestion().is_some(),
        "spec 14.12 access is precisely the case a suggested source change exists for"
    );

    assert!(
        diagnostics.warnings_for(&documented).is_empty(),
        "the plugin using only documented APIs collected {:?}",
        codes(&diagnostics, &documented)
    );
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

#[test]
fn repeated_findings_collapse_per_code_keeping_the_first_detail_and_a_count() {
    let owner = plugin("legacy.example.noisy");
    let other = plugin("legacy.example.quiet");
    let mut diagnostics = diagnostics();

    assert_eq!(
        diagnostics.observe_import(&owner, "win32com.client", missing("first failure")),
        Recorded::Retained,
    );
    assert_eq!(
        diagnostics.observe_import(&owner, "pywintypes", missing("second failure")),
        Recorded::Deduplicated,
        "a second unsupported import folds into the plugin's existing record for that code"
    );
    assert_eq!(
        diagnostics.observe_import(&owner, "win32com.client", missing("third failure")),
        Recorded::Deduplicated,
    );

    let record = record_for(&diagnostics, &owner, "unsupported-import");
    assert_eq!(
        record.occurrences, 3,
        "the count is what tells a developer this is a loop rather than a one-off"
    );
    match &record.warning.issue {
        CompatibilityIssue::UnsupportedImport { module, detail } => {
            assert_eq!(
                module, "win32com.client",
                "the first occurrence is retained; a record that drifts to the newest occurrence \
                 loses the one the developer already started debugging"
            );
            assert_eq!(detail, "first failure");
        }
        other => panic!("classified as {other:?}"),
    }

    // A different code is a different record: collapsing is per code, not per
    // plugin.
    assert_eq!(
        diagnostics.observe_python_requirement(&owner, PythonVersion::new(4, 0, 0), HOST_PYTHON),
        Recorded::Retained,
    );
    assert_eq!(
        codes(&diagnostics, &owner),
        vec!["unsupported-import", "python-version-incompatible"],
        "records are held in first-occurrence order so two reports of one plugin can be diffed"
    );

    // Counts are per plugin as well as per code.
    assert_eq!(
        diagnostics.observe_import(&other, "win32com.client", missing("only failure")),
        Recorded::Retained,
    );
    assert_eq!(
        record_for(&diagnostics, &other, "unsupported-import").occurrences,
        1
    );
    assert_eq!(
        record_for(&diagnostics, &owner, "unsupported-import").occurrences,
        3
    );
    assert_eq!(
        diagnostics.total_warnings(),
        3,
        "three retained records — two for the noisy plugin, one for the quiet one"
    );
}

// ---------------------------------------------------------------------------
// Bounds (roadmap principle 5: everything is bounded)
// ---------------------------------------------------------------------------

#[test]
fn a_plugin_cannot_grow_the_store_without_bound_and_the_overflow_is_itself_reported() {
    let defaults = DiagnosticLimits::default();
    assert_eq!(
        defaults.max_warnings_per_plugin, 64,
        "the default cap has to be a real number: plugin code reaches this store"
    );
    assert_eq!(defaults.max_detail_chars, 512);
    assert_eq!(defaults.long_callback_threshold_ms, 500);

    let limits = DiagnosticLimits {
        max_warnings_per_plugin: 3,
        max_detail_chars: 64,
        long_callback_threshold_ms: 500,
    };
    let flooding = plugin("legacy.example.flooding");
    let verbose = plugin("legacy.example.verbose");
    let mut diagnostics = LegacyDiagnostics::with_limits(limits);
    assert_eq!(diagnostics.limits(), limits);

    let categories = documented_categories();
    let mut expected: Vec<&'static str> = categories
        .iter()
        .take(limits.max_warnings_per_plugin)
        .map(|(_, code, _, _)| *code)
        .collect();
    let dropped = categories.len() - limits.max_warnings_per_plugin;

    for (index, (issue, code, _, _)) in categories.into_iter().enumerate() {
        let outcome = if index < limits.max_warnings_per_plugin {
            Recorded::Retained
        } else {
            Recorded::Dropped
        };
        assert_eq!(
            diagnostics.report(CompatibilityWarning {
                plugin: flooding.clone(),
                issue,
            }),
            outcome,
            "report {index} (`{code}`) against a cap of {}",
            limits.max_warnings_per_plugin,
        );
    }

    expected.push(OVERFLOW_CODE);
    assert_eq!(
        codes(&diagnostics, &flooding),
        expected,
        "the cap keeps the first findings and adds exactly one record saying what it lost"
    );

    let overflow = record_for(&diagnostics, &flooding, OVERFLOW_CODE);
    match &overflow.warning.issue {
        CompatibilityIssue::DiagnosticsOverflow { dropped: reported } => assert_eq!(
            *reported, dropped as u64,
            "an empty report and a truncated one must not look the same"
        ),
        other => panic!("the overflow record carries {other:?}"),
    }
    assert_eq!(overflow.occurrences, dropped as u64);
    assert_eq!(
        overflow.warning.severity(),
        Severity::Warning,
        "losing diagnostics is not informational, but it is the host's problem and not the \
         plugin author's, so it owes no source change"
    );
    assert!(overflow.warning.suggestion().is_none());

    // A code already retained still counts up while the plugin is over its cap:
    // the cap bounds distinct records, not observation.
    assert_eq!(
        diagnostics.observe_import(&flooding, "win32com.client", missing("again")),
        Recorded::Deduplicated,
    );
    assert_eq!(
        record_for(&diagnostics, &flooding, "unsupported-import").occurrences,
        2
    );
    assert_eq!(
        record_for(&diagnostics, &flooding, OVERFLOW_CODE).occurrences,
        dropped as u64,
        "deduplicating into an existing record dropped nothing, so the overflow count must not move"
    );

    // The other way a plugin can push memory around: one enormous detail string.
    let huge = "m".repeat(10_000);
    assert_eq!(
        diagnostics.observe_import(&verbose, &huge, missing(&"d".repeat(10_000))),
        Recorded::Retained,
    );
    match &record_for(&diagnostics, &verbose, "unsupported-import")
        .warning
        .issue
    {
        CompatibilityIssue::UnsupportedImport { module, detail } => {
            for (field, value) in [("module", module), ("detail", detail)] {
                assert_eq!(
                    value.chars().count(),
                    limits.max_detail_chars,
                    "the retained `{field}` was not truncated to the {} character bound",
                    limits.max_detail_chars,
                );
                assert!(
                    value.ends_with('…'),
                    "a truncated `{field}` that does not show it was truncated reads as the whole \
                     value: {value:?}"
                );
            }
        }
        other => panic!("classified as {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

#[test]
fn one_plugins_findings_never_appear_under_another_plugins_id() {
    let alpha = plugin("legacy.example.alpha");
    let beta = plugin("legacy.example.beta");
    let unseen = plugin("legacy.example.unseen");
    let mut diagnostics = diagnostics();

    // Every observation below is a first occurrence, so each must be retained;
    // asserting that here keeps the counts at the end unambiguous.
    assert_eq!(
        diagnostics.observe_import(&alpha, "win32com.client", missing("alpha's import")),
        Recorded::Retained,
    );
    assert_eq!(
        diagnostics.observe_api_access(&alpha, "keypirinha", "_registry", None),
        Recorded::Retained,
    );
    assert_eq!(
        diagnostics.observe_scheduling_profile(&alpha, SchedulingProfile::LegacyStrict),
        Recorded::Retained,
    );
    assert_eq!(
        diagnostics.observe_import(
            &beta,
            "keypirinha_wintypes",
            ImportOutcome::WindowsOnly {
                entry_point: "kernel32".to_owned(),
                detail: WINTYPES_ERROR.to_owned(),
            },
        ),
        Recorded::Retained,
    );

    assert_eq!(
        codes(&diagnostics, &alpha),
        vec![
            "unsupported-import",
            "undocumented-api-access",
            "scheduling-profile"
        ],
    );
    assert_eq!(codes(&diagnostics, &beta), vec!["windows-only-dependency"]);

    for (owner, records) in [
        (&alpha, diagnostics.warnings_for(&alpha)),
        (&beta, diagnostics.warnings_for(&beta)),
    ] {
        for record in records {
            assert_eq!(
                &record.warning.plugin, owner,
                "`{}` is holding a warning belonging to `{}`",
                owner.0, record.warning.plugin.0,
            );
        }
    }

    assert!(
        diagnostics.warnings_for(&unseen).is_empty(),
        "a plugin nobody observed has no findings, and must not inherit anyone else's"
    );
    assert_eq!(
        diagnostics.plugins(),
        vec![alpha.clone(), beta.clone()],
        "the store lists exactly the plugins it observed, in a deterministic order"
    );
    assert_eq!(diagnostics.total_warnings(), 4);

    // Portability is per plugin too: beta's Win32 dependency says nothing about
    // alpha, whose findings are bad but platform-neutral.
    assert!(!diagnostics.is_portable(&beta));
    assert!(
        diagnostics.is_portable(&alpha),
        "acceptance 31.31 is about the plugin that needs Windows, not about every plugin loaded \
         beside it"
    );
}

#[test]
fn a_zero_warning_cap_retains_only_one_bounded_overflow_record() {
    let owner = plugin("legacy.example.zero-cap");
    let mut diagnostics = LegacyDiagnostics::with_limits(DiagnosticLimits {
        max_warnings_per_plugin: 0,
        ..DiagnosticLimits::default()
    });

    for index in 0..10 {
        assert_eq!(
            diagnostics.observe_import(
                &owner,
                &format!("missing.module.{index}"),
                missing("not installed"),
            ),
            Recorded::Dropped,
            "a zero cap must drop every distinct finding",
        );
    }

    assert_eq!(
        diagnostics.total_warnings(),
        1,
        "the bounded overflow notice is the only retained record at a zero cap",
    );
    let records = diagnostics.warnings_for(&owner);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].warning.code(), OVERFLOW_CODE);
    match &records[0].warning.issue {
        CompatibilityIssue::DiagnosticsOverflow { dropped } => assert_eq!(*dropped, 10),
        other => panic!("zero-cap overflow carried {other:?}"),
    }
}
