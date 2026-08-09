//! Modern CPython worker host (spec 15).
//!
//! Python never runs on the UI thread and never inside the UI process. A modern
//! plugin runs in a supervised child interpreter, chosen by [`interpreter`] and
//! pooled by [`host`], speaking the newline-delimited JSON protocol defined in
//! [`protocol`] and driven over the process boundary by [`worker`]. Managed
//! dependency environments — the content-addressed identity that decides which
//! plugins may share a worker — live in `crikey-package-manager` and are
//! re-exported here for consumers that only depend on the host.

use std::path::PathBuf;

mod host;
mod interpreter;
mod protocol;
mod worker;

pub use host::WorkerPool;
pub use interpreter::{
    bundled_interpreter_beside, discover_interpreter, discover_interpreter_in, DiscoveryEnvironment,
    Interpreter, InterpreterSource, PythonVersion, RequiresPython, RuntimeCatalog, BUNDLED_RUNTIME_DIR,
    ENV_PYTHON_OVERRIDE,
};
pub use protocol::{
    MAX_FRAME_BYTES, MAX_LOG_LINES, MAX_LOG_LINE_BYTES, MAX_STDERR_TAIL_BYTES, PROTOCOL_VERSION,
};
pub use worker::{
    BackgroundDiagnostics, BatchState, CancelHandle, ExecuteOutcome, HostError, ModernWorker, PluginError,
    SuggestRequest, Suggestions, WorkerExit, WorkerOptions, ENV_ENTRYPOINT, ENV_PLUGIN_ID,
    ENV_PROTOCOL_VERSION, ENV_SDK_DIR, WORKER_ENTRY_FILE, WORKER_ISOLATION_FLAG,
};

/// Managed-environment types owned by `crikey-package-manager` (spec 15.3,
/// 15.4). Re-exported so a consumer that depends only on the host can name the
/// [`EnvironmentId`] a worker is keyed by and the [`ImportPath`] a worker is
/// launched with, without a second direct dependency.
pub use crikey_package_manager::{EnvironmentId, ImportPath, MaterializedEnvironment};

/// Which interpreter a worker runs (spec 14.11).
///
/// A plugin never names one of these: it declares a `requires-python`, and
/// [`RuntimeCatalog::profile_for`] maps that declaration onto the profile whose
/// interpreter satisfies it. Two plugins with incompatible requirements
/// therefore map to two profiles, two interpreters and — because the
/// interpreter's version is part of a worker's environment identity — two
/// separate child processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeProfile {
    /// Interpreter kept for legacy compatibility.
    LegacyCompatibility,
    /// No interpreter named explicitly: discovery applies its ordered rules,
    /// which prefer the runtime staged beside the executable
    /// ([`BUNDLED_RUNTIME_DIR`]) over one found on the search path. Also the
    /// profile used when `CRIKEY_PYTHON` is set, because that override is
    /// decisive and a profile naming a path would only compete with it.
    Bundled,
    /// A specific interpreter, named by [`RuntimeCatalog::profile_for`] from a
    /// plugin's `requires-python`.
    External(PathBuf),
}

/// Where `_crikey_modern_worker.py` and the `crikey_sdk` package live at run
/// time.
///
/// First hit wins: [`ENV_SDK_DIR`], then `modern-sdk` beside the running
/// executable (the installed layout), then the repository `sdk/python`
/// directory (the development layout). Mirrors the legacy layer's `shim_root`.
///
/// Deliberately does not prove that [`WORKER_ENTRY_FILE`] is present: a caller
/// that wants to fail early with a good message checks
/// `sdk_root().join(WORKER_ENTRY_FILE).is_file()`, and a caller that does not
/// gets the same failure from [`ModernWorker::spawn`].
pub fn sdk_root() -> PathBuf {
    if let Some(configured) = std::env::var_os(ENV_SDK_DIR) {
        return PathBuf::from(configured);
    }

    if let Some(directory) = std::env::current_exe().ok().and_then(|exe| {
        let installed = exe.parent()?.join("modern-sdk");
        installed.join(WORKER_ENTRY_FILE).is_file().then_some(installed)
    }) {
        return directory;
    }

    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../sdk/python"))
}
