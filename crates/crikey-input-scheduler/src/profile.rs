//! Scheduling profiles (spec 7).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchedulingProfile {
    /// Default for unchanged Keypirinha plugins. Time debouncing disabled.
    #[default]
    LegacyStrict,
    /// Opt-in only; documented as potentially behaviour changing.
    LegacyOptimized,
    /// Modern Python and native plugins.
    Modern,
}

impl SchedulingProfile {
    /// Whether the host may apply a time-based debounce interval.
    ///
    /// `legacy-strict` must never be time debounced (spec 8.4, 25.4).
    pub fn allows_time_debounce(self) -> bool {
        !matches!(self, SchedulingProfile::LegacyStrict)
    }

    /// Whether the host may impose a minimum query length or prefix gating.
    pub fn allows_host_gating(self) -> bool {
        !matches!(self, SchedulingProfile::LegacyStrict)
    }

    /// Whether dynamic suggestion results may be cached across requests.
    pub fn allows_dynamic_result_cache(self) -> bool {
        matches!(self, SchedulingProfile::Modern)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SchedulingProfile::LegacyStrict => "legacy-strict",
            SchedulingProfile::LegacyOptimized => "legacy-optimized",
            SchedulingProfile::Modern => "modern",
        }
    }
}
