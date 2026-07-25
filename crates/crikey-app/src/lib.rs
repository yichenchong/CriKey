//! Composition root (spec 5.1).
//!
//! Wires the query scheduler, core services, plugin hosts and the platform
//! backend for the current target. Nothing else in the workspace is allowed to
//! know which backend was selected.

use crikey_core::GenerationTracker;
use crikey_input_scheduler::SchedulingProfile;
use crikey_result_aggregator::ResultLimits;

/// Staged startup (spec 25.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StartupStage {
    WindowAndHotkey,
    PersistedCatalog,
    AcceptQueries,
    RequiredWorkers,
    LegacyPlugins,
    BackgroundRefresh,
}

#[derive(Debug)]
pub struct App {
    generations: GenerationTracker,
    limits: ResultLimits,
    default_legacy_profile: SchedulingProfile,
    stage: StartupStage,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            generations: GenerationTracker::new(),
            limits: ResultLimits::default(),
            default_legacy_profile: SchedulingProfile::LegacyStrict,
            stage: StartupStage::WindowAndHotkey,
        }
    }

    pub fn generations(&self) -> &GenerationTracker {
        &self.generations
    }

    pub fn limits(&self) -> &ResultLimits {
        &self.limits
    }

    pub fn default_legacy_profile(&self) -> SchedulingProfile {
        self.default_legacy_profile
    }

    pub fn stage(&self) -> StartupStage {
        self.stage
    }

    /// Name of the platform backend compiled into this build.
    pub fn platform_backend_name() -> &'static str {
        if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "unsupported"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_legacy_plugins_default_to_legacy_strict() {
        assert_eq!(
            App::new().default_legacy_profile(),
            SchedulingProfile::LegacyStrict
        );
    }
}
