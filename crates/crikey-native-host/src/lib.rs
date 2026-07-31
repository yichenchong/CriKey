//! Native subprocess plugin host (spec 16.1, 16.6).
//!
//! Native plugins are never loaded into the CriKey process.  The host launches
//! each executable with a restricted environment, authenticates a fresh IPC
//! session, and supervises the process boundary.

mod launch;
mod stream;
mod supervisor;
mod worker;

pub use launch::{
    LaunchSpec, LimitEnforcement, ResourceLimitReport, ResourceLimits, TransportKind, WorkerOptions,
};
pub use stream::{
    BatchState, EchoMismatch, ExecuteOutcome, HealthSnapshot, NativeSuggestRequest, PluginError,
    ProtocolObservation, StreamDiagnostics, Suggestions, MAX_LOG_RECORDS, OBSERVATION_CAPACITY,
    READER_QUEUE_CAPACITY, READER_QUEUE_MAX_BYTES,
};
pub use supervisor::{NativeSupervisor, SupervisorConfig};
pub use worker::{CancelHandle, ExitKind, ExitRecord, HostError, NativeWorker, PluginHandshake};
