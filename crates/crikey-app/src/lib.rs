//! Composition root (spec 5.1).
//!
//! Wires the query scheduler, core services, plugin hosts and the platform
//! backend for the current target. Nothing else in the workspace is allowed to
//! know which backend was selected.

use crikey_core::GenerationTracker;
use crikey_input_scheduler::SchedulingProfile;
use crikey_result_aggregator::ResultLimits;

/// State-only milestones for staged startup (spec 25.6).
///
/// Completing a milestone records coordination state only. The caller remains
/// responsible for performing and verifying the corresponding startup work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupStage {
    WindowAndHotkey,
    PersistedCatalog,
    AcceptQueries,
    RequiredWorkers,
    LegacyPlugins,
    BackgroundRefresh,
    LazyModernPlugins,
}

/// A rejected acknowledgement of a startup milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupError {
    /// The acknowledged milestone precedes the milestone currently pending.
    StaleAcknowledgement {
        expected: StartupStage,
        pending: StartupStage,
    },
    /// The acknowledged milestone has not become pending yet.
    OutOfOrderAcknowledgement {
        expected: StartupStage,
        pending: StartupStage,
    },
    /// The eager startup sequence has already handed off to lazy activation.
    AlreadyComplete,
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleAcknowledgement { expected, pending } => write!(
                formatter,
                "startup milestone {expected:?} is stale; {pending:?} is pending"
            ),
            Self::OutOfOrderAcknowledgement { expected, pending } => write!(
                formatter,
                "startup milestone {expected:?} is out of order; {pending:?} is pending"
            ),
            Self::AlreadyComplete => formatter.write_str("eager startup coordination is already complete"),
        }
    }
}

impl std::error::Error for StartupError {}

impl StartupStage {
    /// Returns the next startup stage in specification order.
    pub const fn next(self) -> Option<Self> {
        match self {
            Self::WindowAndHotkey => Some(Self::PersistedCatalog),
            Self::PersistedCatalog => Some(Self::AcceptQueries),
            Self::AcceptQueries => Some(Self::RequiredWorkers),
            Self::RequiredWorkers => Some(Self::LegacyPlugins),
            Self::LegacyPlugins => Some(Self::BackgroundRefresh),
            Self::BackgroundRefresh => Some(Self::LazyModernPlugins),
            Self::LazyModernPlugins => None,
        }
    }

    fn precedes(self, other: Self) -> bool {
        let mut candidate = self.next();
        while let Some(stage) = candidate {
            if stage == other {
                return true;
            }
            candidate = stage.next();
        }
        false
    }
}

#[derive(Debug)]
pub struct App {
    generations: GenerationTracker,
    limits: ResultLimits,
    default_legacy_profile: SchedulingProfile,
    stage: StartupStage,
    eager_startup_complete: bool,
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
            eager_startup_complete: false,
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

    /// Returns the milestone currently awaiting acknowledgement.
    ///
    /// After eager startup completes, the terminal lazy-activation milestone
    /// remains visible for diagnostics.
    pub fn stage(&self) -> StartupStage {
        self.stage
    }

    /// Acknowledges completion of the expected current startup milestone.
    ///
    /// This method coordinates state only: it performs no window, catalog,
    /// worker, plugin, or refresh work. Intermediate acknowledgements return
    /// the next pending milestone. Acknowledging `LazyModernPlugins` returns
    /// `None` and completes the eager sequence, handing responsibility to
    /// demand-driven lazy activation without activating a plugin itself.
    pub fn complete_stage(&mut self, expected: StartupStage) -> Result<Option<StartupStage>, StartupError> {
        if self.eager_startup_complete {
            return Err(StartupError::AlreadyComplete);
        }

        if expected != self.stage {
            let error = if expected.precedes(self.stage) {
                StartupError::StaleAcknowledgement {
                    expected,
                    pending: self.stage,
                }
            } else {
                StartupError::OutOfOrderAcknowledgement {
                    expected,
                    pending: self.stage,
                }
            };
            return Err(error);
        }

        match self.stage.next() {
            Some(next) => {
                self.stage = next;
                Ok(Some(next))
            }
            None => {
                self.eager_startup_complete = true;
                Ok(None)
            }
        }
    }

    /// Whether the acknowledged milestones permit user queries.
    pub fn can_accept_queries(&self) -> bool {
        match self.stage {
            StartupStage::WindowAndHotkey | StartupStage::PersistedCatalog | StartupStage::AcceptQueries => {
                false
            }
            StartupStage::RequiredWorkers
            | StartupStage::LegacyPlugins
            | StartupStage::BackgroundRefresh
            | StartupStage::LazyModernPlugins => true,
        }
    }

    /// Whether eager startup coordination has handed off to lazy activation.
    ///
    /// This does not mean that any lazy modern plugin has been activated.
    pub fn startup_complete(&self) -> bool {
        self.eager_startup_complete
    }

    /// Name of the platform backend compiled into this build.
    pub fn platform_backend_name() -> &'static str {
        Backend::NAME
    }
}

/// The platform backend selected for this target.
///
/// Per ADR-0001 this is the only place in the workspace that names a backend
/// crate. Resolving the alias is what makes the `cfg` target dependencies
/// load-bearing: a mis-gated or renamed backend fails the build here rather
/// than silently falling back.
#[cfg(windows)]
pub type Backend = crikey_platform_windows::WindowsBackend;
#[cfg(target_os = "macos")]
pub type Backend = crikey_platform_macos::MacOsBackend;
#[cfg(target_os = "linux")]
pub type Backend = crikey_platform_linux::LinuxBackend;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
compile_error!(
    "CriKey has no platform backend for this target; \
     implement one behind the crikey-platform traits (spec 18)"
);

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

    #[test]
    fn the_selected_backend_identifies_itself() {
        let _backend = Backend::new();
        assert!(
            matches!(App::platform_backend_name(), "windows" | "macos" | "linux"),
            "backend NAME must be a known platform id, got {:?}",
            App::platform_backend_name()
        );
    }
}
