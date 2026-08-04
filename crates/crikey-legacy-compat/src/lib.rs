//! Legacy Compatibility Layer (spec 14).
//!
//! Implements the documented Keypirinha Python API surface, package formats,
//! lifecycle and `legacy-strict` scheduling. CriKey is an independent project
//! and this layer is not an official Keypirinha component.
//!
//! Every module's public types are re-exported here: callers write
//! `use crikey_legacy_compat::LegacySettings`, never the module path.

pub mod config;
pub mod diagnostics;
pub mod events;
pub mod interpreter;
pub mod lifecycle;
pub mod matrix;
pub mod package;
pub mod worker;

pub use config::{LegacySettings, SettingsError};
pub use diagnostics::{
    CallbackObservation, CompatibilityIssue, CompatibilityWarning, DiagnosticLimits, ImportOutcome,
    LegacyDiagnostics, Recorded, Severity, WarningRecord,
};
pub use events::{
    ActivationState, CallbackOutcome, CoalescerConfig, CoalescerDiagnostics, EventCoalescer, EventDelivery,
    LegacyEventFlags, RawFilesystemKind, RawFilesystemNotification, WatchScope,
};
pub use interpreter::{
    discover_interpreter, discover_interpreter_in, DiscoveryEnvironment, Interpreter, InterpreterSource,
    PythonVersion, MINIMUM_SUPPORTED_PYTHON,
};
pub use lifecycle::{
    CatalogRejectReason, DeadlinePolicy, Delivery, DynamicCachePolicy, LegacyCompatibility, LegacyDeadlines,
    LegacyInstanceState, LegacyPluginDiagnostics, LegacyRegistration, LegacyRuntime, LegacyRuntimeError,
    LegacyTraceEvent, LegacyWorkerHandle, ShutdownReport, TerminationReason, MAX_CATALOG_ITEMS,
    MAX_ITEMS_PER_PUBLICATION, TRACE_CAPACITY,
};
pub use matrix::{
    CompatibilityMatrix, CompatibilityReport, CorpusEntry, MatrixEntry, MatrixError, PluginClassification,
    PluginCorpus,
};
pub use package::{
    LegacyPackage, PackageError, PackageId, PackageLimits, PackageLoader, PackageModule, PackageRoot,
    PACKAGE_ARCHIVE_EXTENSION,
};
pub use worker::{
    shim_root, InstanceId, LegacyOutcome, LegacyRequest, LegacyRequestKind, LegacyResponse, LegacyWorker,
    PluginException, TerminateHandle, WorkerError, WorkerExit, WorkerOptions, ENV_CACHE_DIR, ENV_MAIN_MODULE,
    ENV_MAIN_MODULE_PATH, ENV_PACKAGE_ID, ENV_PACKAGE_ROOT, ENV_PLUGIN_ID, ENV_PROTOCOL_VERSION,
    ENV_SHIM_DIR_OVERRIDE, MAX_FRAME_BYTES, MAX_LOG_LINES, MAX_LOG_LINE_BYTES, MAX_STDERR_TAIL_BYTES,
    PROTOCOL_VERSION, WORKER_ENTRY_FILE, WORKER_ISOLATION_FLAG,
};

use crikey_core::PluginId;
use crikey_input_scheduler::{ObsoleteWorkManager, SchedulingProfile};

/// Documented legacy lifecycle callbacks (spec 13.2). Serialized per instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LegacyCallback {
    OnStart,
    OnCatalog,
    OnSuggest,
    OnExecute,
    OnActivated,
    OnDeactivated,
    OnEvents,
}

impl LegacyCallback {
    /// The documented Keypirinha method name. Diagnostics and the developer
    /// commands print this, so it is part of the observable contract.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnStart => "on_start",
            Self::OnCatalog => "on_catalog",
            Self::OnSuggest => "on_suggest",
            Self::OnExecute => "on_execute",
            Self::OnActivated => "on_activated",
            Self::OnDeactivated => "on_deactivated",
            Self::OnEvents => "on_events",
        }
    }
}

impl std::fmt::Display for LegacyCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Support classification used by the version-controlled compatibility matrix
/// in `compatibility/api-matrix` (spec 14.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiSupport {
    Full,
    BehaviouralDifference,
    WindowsOnly,
    Partial,
    Unsupported,
    Planned,
}

impl ApiSupport {
    /// Declaration order, used to render deterministic reports.
    pub const ALL: [ApiSupport; 6] = [
        Self::Full,
        Self::BehaviouralDifference,
        Self::WindowsOnly,
        Self::Partial,
        Self::Unsupported,
        Self::Planned,
    ];

    /// The kebab-case spelling used in `matrix.toml`.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::BehaviouralDifference => "behavioural-difference",
            Self::WindowsOnly => "windows-only",
            Self::Partial => "partial",
            Self::Unsupported => "unsupported",
            Self::Planned => "planned",
        }
    }

    /// Total and case-sensitive: an unknown spelling is `None`, never a
    /// silently defaulted status.
    pub fn parse_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.slug() == slug)
    }

    /// Whether an API classified this way may be advertised as cross-platform
    /// (spec 31.31). `WindowsOnly` never is.
    pub fn is_portable(self) -> bool {
        matches!(self, Self::Full | Self::BehaviouralDifference | Self::Partial)
    }
}

impl std::fmt::Display for ApiSupport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// Compatibility-only scheduling state for one legacy plugin instance.
///
/// This is **not** the live lifecycle state machine. Production dispatch uses
/// [`crate::lifecycle::LegacyRuntime`], which also tracks initialization,
/// catalog work, reloads, instance identity, and stale-result rejection.
/// Prefer `LegacyRuntime` for all real plugin lifecycle decisions.
#[derive(Debug)]
pub struct LegacyPluginState {
    pub plugin: PluginId,
    pub profile: SchedulingProfile,
    pub dispatch: ObsoleteWorkManager,
}

impl LegacyPluginState {
    pub fn new(plugin: PluginId) -> Self {
        Self {
            plugin,
            profile: SchedulingProfile::LegacyStrict,
            dispatch: ObsoleteWorkManager::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_plugins_default_to_strict_and_are_never_time_debounced() {
        let state = LegacyPluginState::new(PluginId("legacy.example".into()));
        assert_eq!(state.profile, SchedulingProfile::LegacyStrict);
        assert!(!state.profile.allows_time_debounce());
        assert!(!state.profile.allows_dynamic_result_cache());
        assert!(!state.profile.allows_host_gating());
    }

    #[test]
    fn every_support_status_round_trips_through_its_slug() {
        for status in ApiSupport::ALL {
            assert_eq!(
                ApiSupport::parse_slug(status.slug()),
                Some(status),
                "`{}` must round-trip through its kebab-case spelling",
                status.slug()
            );
        }
        assert_eq!(
            ApiSupport::parse_slug("Windows-Only"),
            None,
            "status parsing is case-sensitive so a misspelling is never silently accepted"
        );
    }

    #[test]
    fn windows_only_apis_are_never_advertised_as_portable() {
        assert!(!ApiSupport::WindowsOnly.is_portable());
        assert!(!ApiSupport::Unsupported.is_portable());
        assert!(!ApiSupport::Planned.is_portable());
        assert!(ApiSupport::Full.is_portable());
    }

    #[test]
    fn every_callback_reports_its_documented_keypirinha_name() {
        assert_eq!(LegacyCallback::OnStart.as_str(), "on_start");
        assert_eq!(LegacyCallback::OnEvents.as_str(), "on_events");
        assert_eq!(LegacyCallback::OnSuggest.to_string(), "on_suggest");
    }
}
