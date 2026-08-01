//! Legacy compatibility diagnostics (spec 26.1, 26.2, 14.12; acceptance 31.29,
//! 31.31).
//!
//! Spec 26.2 lists nine things CriKey should report about a legacy plugin.
//! Eight of them are categories of finding; the ninth — "suggested source
//! changes" — is an obligation attached to the others rather than a category of
//! its own. [`CompatibilityIssue`] enumerates the eight (plus one code the store
//! uses to report on *itself*), and [`CompatibilityWarning::suggestion`] is the
//! ninth, present on exactly the findings a plugin author can act on in source.
//!
//! # Why a finding is a value, not a log line
//!
//! Acceptance 31.29 asks for actionable diagnostics. A formatted string cannot
//! be counted, deduplicated, bounded, filtered by plugin, or turned into a
//! compatibility-matrix entry, so every finding is a typed value and the human
//! prose is derived from it on demand. That keeps the stable machine handle
//! ([`CompatibilityWarning::code`]) separate from the prose, which may be
//! reworded without breaking anything that greps for a code.
//!
//! # Why nothing here reads a clock, an interpreter or a worker
//!
//! Diagnostics are a pure fold over observations. The observations themselves —
//! "`keypirinha_wintypes.kernel32` raised `WindowsOnlyError` on this host",
//! "`on_suggest` ran 1,800 ms and never read `should_terminate()`" — are made by
//! the worker and the runtime, which own their own slices. They arrive here as
//! values, so the store is deterministic on every platform.
//!
//! # Bounds
//!
//! Plugin code reaches this store: a plugin in a failure loop can report the
//! same finding thousands of times per second. Two bounds make that harmless,
//! and a third property makes them honest:
//!
//! * Findings deduplicate on `(plugin, code)`, retaining the first occurrence's
//!   detail plus an occurrence count.
//! * Distinct records per plugin are capped by
//!   [`DiagnosticLimits::max_warnings_per_plugin`], and every retained string is
//!   truncated to [`DiagnosticLimits::max_detail_chars`].
//! * Dropping is itself reported, as [`CompatibilityIssue::DiagnosticsOverflow`]
//!   carrying the number lost. Silently discarding findings would make a
//!   truncated report indistinguishable from a clean one.

use std::collections::BTreeMap;
use std::fmt;

use crikey_core::PluginId;
use crikey_input_scheduler::SchedulingProfile;

use crate::{ApiSupport, LegacyCallback, PluginClassification, PythonVersion, MINIMUM_SUPPORTED_PYTHON};

/// How much a finding matters, ordered so a report can be filtered with `>=`.
///
/// `Info` is deliberately reachable: reporting the scheduling profile (spec
/// 26.2) is an observation about how CriKey will run a conforming plugin, not a
/// defect. Giving it the weight of a blocked import would make every legacy
/// plugin look broken and the report would stop being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// A fact about how the plugin will be run. Nothing to fix.
    Info,
    /// The plugin will behave differently here, or has a latent defect.
    Warning,
    /// The plugin cannot work on this host until the source changes.
    Blocking,
}

impl Severity {
    /// The kebab-case spelling used by the developer commands and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Blocking => "blocking",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the store did with an observation.
///
/// `Clean` is distinct from `Deduplicated` on purpose: a caller that cannot tell
/// "this was fine" from "we already knew" cannot report a per-plugin clean bill
/// of health, which is the whole point of acceptance 31.29.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recorded {
    /// The observation is not a finding; nothing was stored.
    Clean,
    /// A new record was created for this `(plugin, code)`.
    Retained,
    /// An existing record for this `(plugin, code)` had its count incremented.
    Deduplicated,
    /// The plugin is at its cap; the finding was lost and counted as lost.
    Dropped,
}

/// What happened when a legacy plugin imported a module (spec 14.2).
///
/// `WindowsOnly` is not a flavour of `Missing`. On a non-Windows host
/// `keypirinha_wintypes` *imports successfully* and each Win32 entry point
/// raises a typed error (spec 14.12), so calling it missing would send the
/// developer off to install something that is already there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    /// The layer provides the module and the plugin can use it here.
    Available,
    /// The interpreter could not resolve the module at all. `detail` is the
    /// interpreter's own words, which are the most useful evidence there is.
    Missing { detail: String },
    /// The module resolved, but the named Win32 entry point refused to run on
    /// this platform. `detail` is the typed refusal the shim raised.
    WindowsOnly { entry_point: String, detail: String },
}

/// One measured legacy callback invocation, as reported by the worker.
///
/// Duration alone is not a defect: spec 9.6 explicitly permits a slow legacy
/// callback and forbids killing the worker for it. What is reportable is a slow
/// callback that cannot be asked to stop, so cooperation is carried alongside
/// the duration rather than inferred from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallbackObservation {
    /// Virtual milliseconds spent inside the callback (spec 25.1 measurement).
    pub duration_ms: u64,
    /// Whether the callback read `should_terminate()` at least once.
    pub observed_should_terminate: bool,
}

/// Bounds on what one plugin may cost the diagnostics store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiagnosticLimits {
    /// Distinct retained records per plugin. Further *distinct* codes are
    /// dropped and counted; codes already retained keep counting occurrences,
    /// because the cap bounds storage and not observation.
    pub max_warnings_per_plugin: usize,
    /// Character cap on every retained string (module names, symbols, details).
    /// Counted in `char`s, never bytes: a byte cap would split a UTF-8 sequence
    /// out of an interpreter traceback.
    pub max_detail_chars: usize,
    /// Milliseconds past which a callback that never read `should_terminate()`
    /// becomes reportable.
    pub long_callback_threshold_ms: u64,
}

impl Default for DiagnosticLimits {
    fn default() -> Self {
        Self {
            // Enough to describe a thoroughly broken plugin once over, small
            // enough that a thousand loaded plugins cannot exhaust memory.
            max_warnings_per_plugin: 64,
            // A traceback line plus context. Longer values are plugin output,
            // not diagnostics.
            max_detail_chars: 512,
            // Spec 25.2's modern hard query deadline is the documented duration
            // a legacy callback is measured against. Spec 9.6 forbids killing
            // the worker at that point; it does not forbid saying so.
            long_callback_threshold_ms: 500,
        }
    }
}

/// A single typed compatibility finding about a legacy plugin (spec 26.2).
///
/// Every variant carries the subject it concerns rather than a rendered
/// sentence, so the same value can drive prose, a suggested source change, a
/// severity and a compatibility-matrix entry without any of them re-parsing the
/// others.
#[derive(Debug, Clone, PartialEq)]
pub enum CompatibilityIssue {
    /// Spec 26.2 "missing API calls": documented and classified in the
    /// compatibility matrix, but not usable here.
    MissingApi {
        module: String,
        symbol: String,
        support: ApiSupport,
    },
    /// Spec 26.2 "unsupported imports": the interpreter could not resolve a
    /// module the plugin asked for.
    UnsupportedImport { module: String, detail: String },
    /// Spec 26.2 "Python-version incompatibilities" (spec 14.11). Carries both
    /// versions: naming only one of them is unactionable.
    PythonVersionIncompatible {
        required: PythonVersion,
        available: PythonVersion,
    },
    /// Spec 26.2 "Windows-only dependencies" (spec 14.2, 14.12). The entry point
    /// is what the developer has to guard, so it is named separately.
    WindowsOnlyDependency {
        module: String,
        entry_point: String,
        detail: String,
    },
    /// Spec 26.2 "native extension requirements": a prebuilt binary CriKey
    /// neither builds nor ships.
    NativeExtensionRequired { module: String, artifact: String },
    /// Spec 26.2 / 14.12 "undocumented API access where detectable". Distinct
    /// from [`Self::MissingApi`] because the answers differ: reaching into an
    /// internal is the plugin's bug, an unimplemented documented API is ours.
    UndocumentedApiAccess { module: String, symbol: String },
    /// Spec 26.2 "scheduling profile". An observation, not a defect.
    SchedulingProfileReported { profile: SchedulingProfile },
    /// Spec 26.2 "long callbacks that do not check `should_terminate()`"
    /// (spec 9.2, 9.6). Carries the measurement, not a rounded verdict.
    LongCallbackWithoutTerminationCheck {
        callback: LegacyCallback,
        duration_ms: u64,
        threshold_ms: u64,
    },
    /// Not a spec 26.2 category: the store reporting that a plugin exhausted its
    /// share of it and `dropped` findings were lost. Without this, an empty
    /// report and a truncated one would look the same.
    DiagnosticsOverflow { dropped: u64 },
    /// Spec 27.4 / acceptance 31.31: the published corpus classifies this
    /// package in a state that does not permit a cross-platform claim.
    ///
    /// Distinct from [`Self::WindowsOnlyDependency`], which is a claim about a
    /// named Win32 entry point and is owed real Win32 evidence. Three quite
    /// different situations withhold a portability claim — a package that is
    /// Windows-only by its own dependencies, one that is blocked everywhere,
    /// and one nobody has exercised — and only the first is about Windows at
    /// all. Carrying the classification keeps the message, the severity and the
    /// suggestion derived from what the corpus actually says.
    DeclaredNonPortable { classification: PluginClassification },
}

impl CompatibilityIssue {
    /// The stable machine-readable handle for this finding.
    ///
    /// These strings are a contract: they are keys in the store, filters in the
    /// developer commands, and — for `undocumented-api-access` — the value the
    /// Python shim puts in `UndocumentedApiError.diagnostic_code` (spec 14.12).
    /// Prose may be reworded; a code never changes.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingApi { .. } => "missing-api",
            Self::UnsupportedImport { .. } => "unsupported-import",
            Self::PythonVersionIncompatible { .. } => "python-version-incompatible",
            Self::WindowsOnlyDependency { .. } => "windows-only-dependency",
            Self::NativeExtensionRequired { .. } => "native-extension-required",
            Self::UndocumentedApiAccess { .. } => "undocumented-api-access",
            Self::SchedulingProfileReported { .. } => "scheduling-profile",
            Self::LongCallbackWithoutTerminationCheck { .. } => "long-callback-without-termination-check",
            Self::DiagnosticsOverflow { .. } => "diagnostics-overflow",
            Self::DeclaredNonPortable { .. } => "declared-non-portable",
        }
    }

    /// Whether a plugin carrying this finding may still be advertised as
    /// cross-platform (acceptance 31.31).
    fn is_portable(&self) -> bool {
        !matches!(
            self,
            Self::WindowsOnlyDependency { .. } | Self::DeclaredNonPortable { .. }
        )
    }

    /// Clamp every retained string to `max_chars`, marking the cut with `…`.
    ///
    /// A plugin controls module names and traceback text, so an unbounded detail
    /// is a memory-growth surface reachable from plugin code. A truncated value
    /// that does not show it was truncated reads as the whole value, hence the
    /// ellipsis rather than a bare cut.
    fn bound_details(&mut self, max_chars: usize) {
        match self {
            Self::MissingApi { module, symbol, .. } | Self::UndocumentedApiAccess { module, symbol } => {
                bound(module, max_chars);
                bound(symbol, max_chars);
            }
            Self::UnsupportedImport { module, detail } => {
                bound(module, max_chars);
                bound(detail, max_chars);
            }
            Self::WindowsOnlyDependency {
                module,
                entry_point,
                detail,
            } => {
                bound(module, max_chars);
                bound(entry_point, max_chars);
                bound(detail, max_chars);
            }
            Self::NativeExtensionRequired { module, artifact } => {
                bound(module, max_chars);
                bound(artifact, max_chars);
            }
            // Nothing plugin-controlled and unbounded: these carry only versions,
            // enums and counters.
            Self::PythonVersionIncompatible { .. }
            | Self::SchedulingProfileReported { .. }
            | Self::LongCallbackWithoutTerminationCheck { .. }
            | Self::DeclaredNonPortable { .. }
            | Self::DiagnosticsOverflow { .. } => {}
        }
    }
}

/// Truncate `value` to at most `max_chars` characters, ending with `…`.
fn bound(value: &mut String, max_chars: usize) {
    // The common case is a short module name, and `chars().count()` is the only
    // way to know that without scanning twice; the scan is paid once, on values
    // that are already bounded by the check below.
    if value.chars().count() <= max_chars {
        return;
    }
    if max_chars == 0 {
        value.clear();
        return;
    }
    let cut = value
        .char_indices()
        .nth(max_chars - 1)
        .map_or(value.len(), |(offset, _)| offset);
    value.truncate(cut);
    value.push('…');
}

/// A [`CompatibilityIssue`] attributed to the plugin it concerns.
///
/// The plugin id travels with the finding rather than being implied by where it
/// is stored, so a warning handed to a renderer, a matrix builder or a log sink
/// can still be routed to the developer who can act on it.
#[derive(Debug, Clone, PartialEq)]
pub struct CompatibilityWarning {
    pub plugin: PluginId,
    pub issue: CompatibilityIssue,
}

impl CompatibilityWarning {
    /// The stable machine-readable handle. See [`CompatibilityIssue::code`].
    pub fn code(&self) -> &'static str {
        self.issue.code()
    }

    /// Human-readable prose naming the subject of the finding (spec 26.1).
    ///
    /// Derived on demand rather than stored: the store keeps values, and prose
    /// is a rendering of a value that translations and reports may vary.
    pub fn message(&self) -> String {
        match &self.issue {
            CompatibilityIssue::MissingApi {
                module,
                symbol,
                support,
            } => format!(
                "`{module}.{symbol}` is not available in the legacy compatibility layer; the \
                 compatibility matrix classifies it as `{support}`"
            ),
            CompatibilityIssue::UnsupportedImport { module, detail } => format!(
                "the plugin imports `{module}`, which the legacy compatibility layer does not \
                 provide: {detail}"
            ),
            CompatibilityIssue::PythonVersionIncompatible { required, available } => format!(
                "the plugin requires CPython {required}, but this host runs {available} and \
                 CriKey supports {MINIMUM_SUPPORTED_PYTHON} or newer"
            ),
            CompatibilityIssue::WindowsOnlyDependency {
                module,
                entry_point,
                detail,
            } => format!(
                "the plugin depends on `{module}.{entry_point}`, a Windows-only Win32 entry \
                 point: {detail}"
            ),
            CompatibilityIssue::NativeExtensionRequired { module, artifact } => format!(
                "the plugin needs the prebuilt native extension `{artifact}` for `{module}`, \
                 which CriKey neither builds nor ships"
            ),
            CompatibilityIssue::UndocumentedApiAccess { module, symbol } => format!(
                "the plugin reads `{module}.{symbol}`, which is undocumented and absent from the \
                 compatibility matrix"
            ),
            CompatibilityIssue::SchedulingProfileReported { profile } => format!(
                "the plugin runs under the `{}` scheduling profile",
                profile.as_str()
            ),
            CompatibilityIssue::LongCallbackWithoutTerminationCheck {
                callback,
                duration_ms,
                threshold_ms,
            } => format!(
                "`{callback}` ran for {duration_ms} ms, past the {threshold_ms} ms threshold, \
                 without ever reading `should_terminate()`"
            ),
            CompatibilityIssue::DiagnosticsOverflow { dropped } => format!(
                "{dropped} further compatibility findings for this plugin were dropped after it \
                 exhausted its diagnostics budget"
            ),
            CompatibilityIssue::DeclaredNonPortable { classification } => format!(
                "the published corpus classifies this package `{classification}` (spec 27.4), \
                 which does not permit a cross-platform claim: {}",
                declared_reason(*classification)
            ),
        }
    }

    /// Spec 26.2's "suggested source changes", where one exists.
    ///
    /// A suggestion is owed wherever a plugin author could change source to
    /// resolve the finding. It is *not* owed for `scheduling-profile`, which is
    /// a fact about how CriKey runs the plugin rather than a defect, nor for
    /// `diagnostics-overflow`, which is the host's problem and not the author's.
    /// Inventing a suggestion for those two would train developers to ignore the
    /// field.
    pub fn suggestion(&self) -> Option<String> {
        match &self.issue {
            CompatibilityIssue::MissingApi {
                module,
                symbol,
                support,
            } => Some(format!(
                "`{module}.{symbol}` is classified `{support}`; guard the call with `hasattr` and \
                 fall back, or drop the feature that needs it"
            )),
            CompatibilityIssue::UnsupportedImport { module, .. } => Some(format!(
                "remove `import {module}` or wrap it in `try`/`except ImportError` and degrade \
                 gracefully; the layer provides only the documented `keypirinha`, \
                 `keypirinha_util`, `keypirinha_net` and `keypirinha_wintypes` modules (spec 14.2)"
            )),
            CompatibilityIssue::PythonVersionIncompatible { required, available } => Some(format!(
                "target CPython {MINIMUM_SUPPORTED_PYTHON} or newer and no newer than the \
                 {available} this host runs; remove the syntax or standard-library use that \
                 needs {required}"
            )),
            CompatibilityIssue::WindowsOnlyDependency {
                module, entry_point, ..
            } => Some(format!(
                "guard `{module}.{entry_point}` behind a platform check and provide a \
                 non-Windows path, or declare the plugin Windows-only"
            )),
            CompatibilityIssue::NativeExtensionRequired { module, artifact } => Some(format!(
                "replace `{module}` with a pure-Python dependency, or ship a build of \
                 `{artifact}` for this platform alongside the package"
            )),
            CompatibilityIssue::UndocumentedApiAccess { module, symbol } => Some(format!(
                "replace the use of the undocumented `{module}.{symbol}` with a documented API \
                 from the compatibility matrix (spec 14.12)"
            )),
            CompatibilityIssue::LongCallbackWithoutTerminationCheck { callback, .. } => Some(format!(
                "poll `should_terminate()` inside the long-running loop in `{callback}` and \
                     return promptly once it is set (spec 9.4)"
            )),
            CompatibilityIssue::DeclaredNonPortable { classification } => {
                declared_suggestion(*classification).map(str::to_owned)
            }
            CompatibilityIssue::SchedulingProfileReported { .. }
            | CompatibilityIssue::DiagnosticsOverflow { .. } => None,
        }
    }

    /// How much this finding matters (spec 26.1).
    pub fn severity(&self) -> Severity {
        match &self.issue {
            // An unresolvable import or an unmeetable interpreter requirement
            // aborts module load: the plugin cannot run here at all.
            CompatibilityIssue::UnsupportedImport { .. }
            | CompatibilityIssue::PythonVersionIncompatible { .. }
            | CompatibilityIssue::NativeExtensionRequired { .. } => Severity::Blocking,
            // `Unsupported` is a decision not to implement, so the call will
            // never work; `Partial` and `Planned` are gaps a guarded plugin can
            // survive today and that may close later.
            CompatibilityIssue::MissingApi { support, .. } => match support {
                ApiSupport::Unsupported => Severity::Blocking,
                _ => Severity::Warning,
            },
            // Not blocking: the module imports and the plugin may well guard the
            // Win32 call. It is still never portable — see `is_portable`.
            CompatibilityIssue::WindowsOnlyDependency { .. }
            // The plugin's own bug, and often reached through `getattr` with a
            // default, so it degrades rather than fails.
            | CompatibilityIssue::UndocumentedApiAccess { .. }
            // Spec 9.6 forbids killing the worker for this, so it cannot block.
            | CompatibilityIssue::LongCallbackWithoutTerminationCheck { .. }
            // Losing diagnostics is real, but it is the host's problem.
            | CompatibilityIssue::DiagnosticsOverflow { .. } => Severity::Warning,
            // A declared classification is as serious as what it declares: a
            // package blocked on every platform cannot run, one that is
            // Windows-only runs somewhere, and one nobody has audited is a
            // coverage gap rather than a defect. Collapsing the three into one
            // weight would either cry wolf over `untested` or bury a package
            // that loads nowhere.
            CompatibilityIssue::DeclaredNonPortable { classification } => match classification {
                PluginClassification::BlockedMissingApis
                | PluginClassification::BlockedPythonVersion
                | PluginClassification::BlockedUndocumentedBehaviour => Severity::Blocking,
                PluginClassification::WindowsOnlyButCompatible => Severity::Warning,
                // `untested` is an absence of evidence. The remaining spellings
                // are portable and never reach this variant through
                // `observe_declared_classification`; if one is constructed by
                // hand, reporting it as an observation rather than a defect is
                // the honest reading of a state that asserts nothing wrong.
                PluginClassification::Untested
                | PluginClassification::WorksUnchanged
                | PluginClassification::WorksWithConfigurationChanges
                | PluginClassification::WorksWithMinimalSourceChanges
                | PluginClassification::WorksOnlyUnderLegacyOptimized
                | PluginClassification::RequiresLegacyStrict => Severity::Info,
            },
            CompatibilityIssue::SchedulingProfileReported { .. } => Severity::Info,
        }
    }
}

/// Why `classification` withholds a cross-platform claim, as the clause that
/// completes [`CompatibilityWarning::message`].
///
/// One sentence per reason rather than one per state, because the reasons are
/// what differ: naming a Win32 dependency for a package that is blocked on
/// unimplemented APIs would send a developer looking for Windows code that is
/// not there.
fn declared_reason(classification: PluginClassification) -> &'static str {
    match classification {
        PluginClassification::WindowsOnlyButCompatible => {
            "the package depends on Windows itself, which is a property of the package rather \
             than a gap in the compatibility layer"
        }
        PluginClassification::BlockedMissingApis => {
            "the package is blocked on legacy APIs CriKey has not implemented, so it runs on no \
             platform yet"
        }
        PluginClassification::BlockedPythonVersion => {
            "the package requires an interpreter CriKey does not support, so it runs on no \
             platform yet"
        }
        PluginClassification::BlockedUndocumentedBehaviour => {
            "the package relies on undocumented Keypirinha behaviour CriKey does not reproduce, \
             so it runs on no platform yet"
        }
        PluginClassification::Untested => {
            "the package has never been exercised, and portability is a claim that must be \
             earned rather than defaulted into"
        }
        // Unreachable through `observe_declared_classification`, which files
        // nothing for a portable state; total rather than a panic because a
        // diagnostics store may never abort a report over its own input.
        PluginClassification::WorksUnchanged
        | PluginClassification::WorksWithConfigurationChanges
        | PluginClassification::WorksWithMinimalSourceChanges
        | PluginClassification::WorksOnlyUnderLegacyOptimized
        | PluginClassification::RequiresLegacyStrict => {
            "no reason is recorded: this classification does permit a cross-platform claim"
        }
    }
}

/// The action that would resolve a declared classification, where one exists.
///
/// `untested` has none: no source change makes an unaudited package audited,
/// and the work belongs to whoever maintains the corpus. The portable spellings
/// have none because there is nothing to resolve.
fn declared_suggestion(classification: PluginClassification) -> Option<&'static str> {
    match classification {
        PluginClassification::WindowsOnlyButCompatible => Some(
            "provide a non-Windows path for the package's own platform dependency, or keep it \
             declared Windows-only and never advertise it as portable",
        ),
        PluginClassification::BlockedMissingApis
        | PluginClassification::BlockedPythonVersion
        | PluginClassification::BlockedUndocumentedBehaviour => Some(
            "the corpus entry's notes name what blocks this package; resolve that and re-audit \
             it at a new pinned revision (spec 27.4)",
        ),
        PluginClassification::Untested
        | PluginClassification::WorksUnchanged
        | PluginClassification::WorksWithConfigurationChanges
        | PluginClassification::WorksWithMinimalSourceChanges
        | PluginClassification::WorksOnlyUnderLegacyOptimized
        | PluginClassification::RequiresLegacyStrict => None,
    }
}

/// One deduplicated finding plus how often it was observed.
///
/// The retained `warning` is the *first* occurrence. A record that drifted to
/// the newest occurrence would keep losing the one the developer had already
/// started debugging; the count is what tells them it is a loop rather than a
/// one-off.
#[derive(Debug, Clone, PartialEq)]
pub struct WarningRecord {
    pub warning: CompatibilityWarning,
    pub occurrences: u64,
}

/// Per-plugin store of legacy compatibility findings (spec 26.1, 26.2).
///
/// Isolation is absolute: a finding is filed under the plugin named in its
/// warning and is never visible under another. A `BTreeMap` keys the store so
/// [`LegacyDiagnostics::plugins`] is sorted and deterministic without a sort at
/// every call — a report that reorders between runs cannot be diffed.
#[derive(Debug, Clone)]
pub struct LegacyDiagnostics {
    by_plugin: BTreeMap<PluginId, Vec<WarningRecord>>,
    limits: DiagnosticLimits,
}

impl Default for LegacyDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl LegacyDiagnostics {
    /// An empty store with [`DiagnosticLimits::default`].
    pub fn new() -> Self {
        Self::with_limits(DiagnosticLimits::default())
    }

    /// An empty store with explicit bounds.
    pub fn with_limits(limits: DiagnosticLimits) -> Self {
        Self {
            by_plugin: BTreeMap::new(),
            limits,
        }
    }

    /// The bounds in force.
    pub fn limits(&self) -> DiagnosticLimits {
        self.limits
    }

    /// File a finding, deduplicating on `(plugin, code)` and enforcing the cap.
    ///
    /// This is the single choke point: every `observe_*` helper funnels through
    /// it, so truncation, deduplication and the overflow accounting cannot be
    /// bypassed by adding another entry point later.
    pub fn report(&mut self, mut warning: CompatibilityWarning) -> Recorded {
        warning.issue.bound_details(self.limits.max_detail_chars);
        let code = warning.code();
        let cap = self.limits.max_warnings_per_plugin;

        // A key is created only for a plugin that actually has a finding, so
        // `plugins()` lists the plugins observed rather than the ones mentioned.
        // Two lookups beat `entry(plugin.clone())`, which would clone the id on
        // every repeat report of an already-known plugin.
        if !self.by_plugin.contains_key(&warning.plugin) {
            self.by_plugin.insert(warning.plugin.clone(), Vec::new());
        }
        let records = self
            .by_plugin
            .get_mut(&warning.plugin)
            .expect("the entry was inserted above if it was absent");

        if let Some(existing) = records.iter_mut().find(|record| record.warning.code() == code) {
            existing.occurrences = existing.occurrences.saturating_add(1);
            note_overflow_count(existing);
            return Recorded::Deduplicated;
        }

        // The overflow record is exempt from the cap: a plugin that has exhausted
        // its budget is exactly the plugin that must be told so, and a record
        // that could itself be dropped would restore the silent-truncation bug it
        // exists to prevent.
        let is_overflow = matches!(warning.issue, CompatibilityIssue::DiagnosticsOverflow { .. });
        if records.len() >= cap && !is_overflow {
            record_drop(records, &warning.plugin);
            return Recorded::Dropped;
        }

        records.push(WarningRecord {
            warning,
            occurrences: 1,
        });
        Recorded::Retained
    }

    /// Fold in what happened when `owner` imported `module` (spec 14.2, 26.2).
    pub fn observe_import(&mut self, owner: &PluginId, module: &str, outcome: ImportOutcome) -> Recorded {
        let issue = match outcome {
            ImportOutcome::Available => return Recorded::Clean,
            ImportOutcome::Missing { detail } => CompatibilityIssue::UnsupportedImport {
                module: module.to_owned(),
                detail,
            },
            ImportOutcome::WindowsOnly { entry_point, detail } => CompatibilityIssue::WindowsOnlyDependency {
                module: module.to_owned(),
                entry_point,
                detail,
            },
        };
        self.report(CompatibilityWarning {
            plugin: owner.clone(),
            issue,
        })
    }

    /// Fold in `owner` touching `module.symbol`, classified by the compatibility
    /// matrix as `support` (spec 14.10, 14.12, 26.2).
    ///
    /// `None` means the symbol is not in the matrix at all. Spec 14.12 requires a
    /// *specific* diagnostic for that, distinguishable from "documented but not
    /// implemented yet", so it is never folded into `missing-api`.
    pub fn observe_api_access(
        &mut self,
        owner: &PluginId,
        module: &str,
        symbol: &str,
        support: Option<ApiSupport>,
    ) -> Recorded {
        let issue = match support {
            // Classified `full` or `behavioural-difference`: the call works here,
            // and a behavioural difference is a property of the matrix entry, not
            // a defect in this plugin.
            Some(ApiSupport::Full | ApiSupport::BehaviouralDifference) => return Recorded::Clean,
            // Reachable from the matrix alone: a plugin naming a Windows-only
            // symbol is Windows-only whether or not it got as far as calling it.
            Some(ApiSupport::WindowsOnly) => CompatibilityIssue::WindowsOnlyDependency {
                module: module.to_owned(),
                entry_point: symbol.to_owned(),
                detail: format!(
                    "the compatibility matrix classifies `{module}.{symbol}` as `{}` (spec 14.10)",
                    ApiSupport::WindowsOnly
                ),
            },
            Some(support) => CompatibilityIssue::MissingApi {
                module: module.to_owned(),
                symbol: symbol.to_owned(),
                support,
            },
            None => CompatibilityIssue::UndocumentedApiAccess {
                module: module.to_owned(),
                symbol: symbol.to_owned(),
            },
        };
        self.report(CompatibilityWarning {
            plugin: owner.clone(),
            issue,
        })
    }

    /// Fold in the interpreter version `owner` asks for against the one this
    /// host offers (spec 14.11, 26.2).
    ///
    /// "Outside the supported range" has two sides. Asking for more than the host
    /// can offer is the obvious one; asking for less than
    /// [`MINIMUM_SUPPORTED_PYTHON`] is the other, and a newer interpreter does
    /// not satisfy it — a Python 2 plugin does not run on CPython 3.14.
    pub fn observe_python_requirement(
        &mut self,
        owner: &PluginId,
        required: PythonVersion,
        available: PythonVersion,
    ) -> Recorded {
        if required >= MINIMUM_SUPPORTED_PYTHON && required <= available {
            return Recorded::Clean;
        }
        self.report(CompatibilityWarning {
            plugin: owner.clone(),
            issue: CompatibilityIssue::PythonVersionIncompatible { required, available },
        })
    }

    /// Fold in one measured callback invocation (spec 9.2, 9.6, 25.2, 26.2).
    ///
    /// Reportable only when both halves hold: it ran past the threshold *and* it
    /// never read `should_terminate()`. Either alone is permitted behaviour, and
    /// reporting it would bury the case that actually costs the user a stalled
    /// query.
    pub fn observe_callback(
        &mut self,
        owner: &PluginId,
        callback: LegacyCallback,
        observation: CallbackObservation,
    ) -> Recorded {
        let threshold_ms = self.limits.long_callback_threshold_ms;
        if observation.observed_should_terminate || observation.duration_ms <= threshold_ms {
            return Recorded::Clean;
        }
        self.report(CompatibilityWarning {
            plugin: owner.clone(),
            issue: CompatibilityIssue::LongCallbackWithoutTerminationCheck {
                callback,
                duration_ms: observation.duration_ms,
                threshold_ms,
            },
        })
    }

    /// Record the scheduling profile `owner` runs under (spec 7, 26.2).
    ///
    /// Always a finding, never a defect: spec 26.2 asks for the profile to be
    /// reported, and [`Severity::Info`] is how the report says so without making
    /// a conforming plugin look broken.
    pub fn observe_scheduling_profile(&mut self, owner: &PluginId, profile: SchedulingProfile) -> Recorded {
        self.report(CompatibilityWarning {
            plugin: owner.clone(),
            issue: CompatibilityIssue::SchedulingProfileReported { profile },
        })
    }

    /// Fold in the classification the published corpus already declares for
    /// `owner`, independently of anything this host observed (spec 27.4;
    /// acceptance 31.31).
    ///
    /// Every other `observe_*` entry point folds in something that *happened*.
    /// This one exists because acceptance 31.31 is violated by nothing
    /// happening: [`Self::is_portable`] answers over the findings on file, so a
    /// package the corpus documents as Windows-only would read as portable on
    /// any host that simply never ran its Win32 branch — a cross-platform claim
    /// obtained by not looking. Folding the declaration in makes the documented
    /// limitation reach the verdict on its own.
    ///
    /// A classification that permits a cross-platform claim files nothing
    /// ([`Recorded::Clean`]): a report that invents findings for healthy
    /// packages is a report developers learn to skip.
    pub fn observe_declared_classification(
        &mut self,
        owner: &PluginId,
        classification: PluginClassification,
    ) -> Recorded {
        if classification.is_portable() {
            return Recorded::Clean;
        }
        // Filed under its own code, never as a Win32 dependency. Only one of
        // the non-portable states is about Windows at all, and the corpus's
        // Windows-only packages reach Win32 through their own bundled COM,
        // `ctypes` and `sc.exe` rather than through `keypirinha_wintypes` — so
        // naming that module here would be a §14.12 diagnostic about a
        // dependency the package does not have. `windows-only-dependency` stays
        // reserved for an entry point somebody actually observed or matched.
        self.report(CompatibilityWarning {
            plugin: owner.clone(),
            issue: CompatibilityIssue::DeclaredNonPortable { classification },
        })
    }

    /// Everything held for `owner`, in first-occurrence order.
    ///
    /// Stable ordering is what lets two runs of the same plugin be diffed. An
    /// unobserved plugin yields an empty slice rather than an `Option`: "no
    /// findings" and "never seen" are the same answer to this question.
    pub fn warnings_for(&self, owner: &PluginId) -> &[WarningRecord] {
        match self.by_plugin.get(owner) {
            Some(records) => records.as_slice(),
            None => &[],
        }
    }

    /// Total retained records across every plugin. Counts records, not
    /// occurrences: it is the store's size, not the plugins' noisiness.
    pub fn total_warnings(&self) -> usize {
        self.by_plugin.values().map(Vec::len).sum()
    }

    /// Whether `owner` may be advertised as cross-platform (acceptance 31.31).
    ///
    /// Per plugin, and false only for an observed Windows-only dependency or a
    /// declared non-portable classification: the other findings are bad but
    /// platform-neutral, and one plugin's limitation says nothing about the
    /// plugins loaded beside it.
    pub fn is_portable(&self, owner: &PluginId) -> bool {
        self.warnings_for(owner)
            .iter()
            .all(|record| record.warning.issue.is_portable())
    }

    /// Every plugin with at least one finding, sorted by id.
    pub fn plugins(&self) -> Vec<PluginId> {
        self.by_plugin.keys().cloned().collect()
    }
}

/// Keep [`CompatibilityIssue::DiagnosticsOverflow`]'s payload equal to its count.
///
/// Deduplication otherwise freezes the first occurrence's value, which is right
/// for every other issue and wrong for this one: the overflow record's whole
/// content *is* how many findings were lost so far.
fn note_overflow_count(record: &mut WarningRecord) {
    let seen = record.occurrences;
    if let CompatibilityIssue::DiagnosticsOverflow { dropped } = &mut record.warning.issue {
        *dropped = seen;
    }
}

/// Account for one dropped finding against `owner`'s overflow record, creating
/// it if this is the first drop.
fn record_drop(records: &mut Vec<WarningRecord>, owner: &PluginId) {
    if let Some(existing) = records.iter_mut().find(|record| {
        matches!(
            record.warning.issue,
            CompatibilityIssue::DiagnosticsOverflow { .. }
        )
    }) {
        existing.occurrences = existing.occurrences.saturating_add(1);
        note_overflow_count(existing);
        return;
    }

    records.push(WarningRecord {
        warning: CompatibilityWarning {
            plugin: owner.clone(),
            issue: CompatibilityIssue::DiagnosticsOverflow { dropped: 1 },
        },
        occurrences: 1,
    });
}
