//! Legacy Compatibility Layer (spec 14).
//!
//! Implements the documented Keypirinha Python API surface, package formats,
//! lifecycle and `legacy-strict` scheduling. CriKey is an independent project
//! and this layer is not an official Keypirinha component.

use crikey_core::PluginId;
use crikey_input_scheduler::{ObsoleteWorkManager, SchedulingProfile};

/// Package formats accepted by the loader (spec 14.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyPackage {
    /// A loose Keypirinha package directory.
    Directory(std::path::PathBuf),
    /// A `.keypirinha-package` archive.
    Archive(std::path::PathBuf),
}

/// Documented legacy lifecycle callbacks (spec 13.2). Serialized per instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyCallback {
    OnStart,
    OnCatalog,
    OnSuggest,
    OnExecute,
    OnActivated,
    OnDeactivated,
    OnEvents,
}

/// Support classification used by the version-controlled compatibility matrix
/// in `compatibility/api-matrix` (spec 14.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiSupport {
    Full,
    BehaviouralDifference,
    WindowsOnly,
    Partial,
    Unsupported,
    Planned,
}

/// Scheduling state for one legacy plugin instance.
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
}
