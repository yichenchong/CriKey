//! Plugin supervision (spec 5.2, 13, 24).

use std::time::Duration;

use crikey_core::PluginId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    NotStarted,
    Starting,
    Ready,
    Busy,
    Restarting,
    Suspended,
    Failed,
}

/// Deadlines differ by runtime; legacy callbacks are never hard killed on the
/// modern 500 ms budget (spec 9.6, 25.2, 25.3).
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    pub soft: Duration,
    pub hard: Option<Duration>,
    /// Watchdog for a genuinely hung worker, used for legacy recovery only.
    pub hung_worker: Duration,
}

impl Deadlines {
    pub fn modern_native() -> Self {
        Self {
            soft: Duration::from_millis(50),
            hard: Some(Duration::from_millis(500)),
            hung_worker: Duration::from_secs(30),
        }
    }

    pub fn modern_python() -> Self {
        Self {
            soft: Duration::from_millis(100),
            hard: Some(Duration::from_millis(500)),
            hung_worker: Duration::from_secs(30),
        }
    }

    pub fn legacy() -> Self {
        Self {
            soft: Duration::from_millis(250),
            hard: None,
            hung_worker: Duration::from_secs(60),
        }
    }
}

/// Health counters surfaced by diagnostics (spec 24.3).
#[derive(Debug, Clone, Copy, Default)]
pub struct PluginHealth {
    pub startup_failures: u32,
    pub crashes: u32,
    pub timeouts: u32,
    pub cancellations_honoured: u64,
    pub cancellations_ignored: u64,
    pub stale_results_rejected: u64,
    pub obsolete_requests_dropped: u64,
    pub queue_depth: u32,
    pub peak_latency_ms: u64,
}

/// Suspends a plugin after repeated failures (spec 13.7).
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,
    pub cooldown: Duration,
}

pub trait Supervisor {
    fn start(&mut self, plugin: &PluginId) -> crikey_core::Result<()>;
    fn stop(&mut self, plugin: &PluginId) -> crikey_core::Result<()>;
    fn restart(&mut self, plugin: &PluginId) -> crikey_core::Result<()>;
    fn state(&self, plugin: &PluginId) -> WorkerState;
    fn health(&self, plugin: &PluginId) -> PluginHealth;
}
