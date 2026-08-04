//! Launch configuration for supervised native plugins (spec 16.6).

use std::path::PathBuf;
use std::process::Command;

use crikey_core::PluginId;

/// Executable and restricted environment used for one native plugin process.
///
/// The host owns the endpoint and session token.  They are deliberately not
/// caller-supplied so every spawn gets a fresh authenticated channel.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub plugin: PluginId,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub working_dir: Option<PathBuf>,
    /// Extra variables on top of the restricted base environment (spec 16.6).
    pub environment: Vec<(String, String)>,
}

/// IPC transport selected for one native worker (spec 16.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    UnixSocket,
    NamedPipe,
    Stdio,
}

impl Default for TransportKind {
    #[cfg(unix)]
    fn default() -> Self {
        Self::UnixSocket
    }

    #[cfg(all(not(unix), windows))]
    fn default() -> Self {
        Self::NamedPipe
    }

    #[cfg(all(not(unix), not(windows)))]
    fn default() -> Self {
        Self::Stdio
    }
}

/// Aggregate bounds enforced by the host while consuming plugin streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    pub max_items_per_query: usize,
    pub max_batches_per_query: usize,
    pub max_bytes_per_query: usize,
    pub max_catalog_items: usize,
    pub max_log_bytes: usize,
    pub initial_credits: u32,
    /// Maximum address-space bytes for one native plugin process.
    ///
    /// `None` leaves the operating-system limit unchanged.
    pub max_memory_bytes: Option<u64>,
    /// Maximum consumed CPU time in seconds for one native plugin process.
    ///
    /// `None` leaves the operating-system limit unchanged.
    pub max_cpu_time_seconds: Option<u64>,
    /// Maximum number of processes the native plugin may create.
    ///
    /// `None` leaves the operating-system limit unchanged.
    pub max_processes: Option<u64>,
    /// Maximum number of open file descriptors/handles for the native plugin.
    ///
    /// `None` leaves the operating-system limit unchanged.
    pub max_open_files: Option<u64>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_items_per_query: 10_000,
            max_batches_per_query: 512,
            max_bytes_per_query: 32 * 1024 * 1024,
            max_catalog_items: 500_000,
            max_log_bytes: 1024 * 1024,
            initial_credits: 8,
            max_memory_bytes: None,
            max_cpu_time_seconds: None,
            max_processes: None,
            max_open_files: None,
        }
    }
}

/// Whether one operating-system limit is configured and enforceable (spec 24.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitEnforcement {
    /// The caller did not request this limit.
    NotConfigured,
    /// The host applies this limit to the child process.
    Enforced,
    /// This target cannot enforce this limit through its process controls.
    Unavailable(&'static str),
}

/// Honest per-limit capability report for a launch target (spec 24.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimitReport {
    pub memory: LimitEnforcement,
    pub cpu_time: LimitEnforcement,
    pub process_count: LimitEnforcement,
    pub open_files: LimitEnforcement,
}

impl ResourceLimits {
    /// Reports what this target can enforce without claiming unavailable limits.
    pub fn platform_report(&self) -> ResourceLimitReport {
        ResourceLimitReport {
            memory: self.limit_status(self.max_memory_bytes, LimitName::Memory),
            cpu_time: self.limit_status(self.max_cpu_time_seconds, LimitName::CpuTime),
            process_count: self.limit_status(self.max_processes, LimitName::ProcessCount),
            open_files: self.limit_status(self.max_open_files, LimitName::OpenFiles),
        }
    }

    fn limit_status(&self, value: Option<u64>, name: LimitName) -> LimitEnforcement {
        if value.is_none() {
            return LimitEnforcement::NotConfigured;
        }
        if name.is_supported() {
            LimitEnforcement::Enforced
        } else {
            LimitEnforcement::Unavailable(name.unavailable_reason())
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LimitName {
    Memory,
    CpuTime,
    ProcessCount,
    OpenFiles,
}

impl LimitName {
    fn is_supported(self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let _ = self;
            true
        }
        #[cfg(windows)]
        {
            !matches!(self, Self::OpenFiles)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = self;
            false
        }
    }

    fn unavailable_reason(self) -> &'static str {
        match self {
            Self::Memory | Self::CpuTime | Self::ProcessCount => {
                "this platform has no native per-process limit"
            }
            Self::OpenFiles => "this platform has no native per-plugin open-file limit",
        }
    }
}

/// Timeouts and stream limits fixed before a worker is spawned.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    pub transport: TransportKind,
    pub startup_timeout_ms: u64,
    pub call_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
    pub limits: ResourceLimits,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerOptions {
    /// Returns the contract defaults.
    pub fn new() -> Self {
        Self {
            transport: TransportKind::default(),
            startup_timeout_ms: 10_000,
            call_timeout_ms: 5_000,
            shutdown_timeout_ms: 2_000,
            limits: ResourceLimits::default(),
        }
    }

    /// Sets the transport using a chainable builder.
    pub fn with_transport(mut self, transport: TransportKind) -> Self {
        self.transport = transport;
        self
    }

    /// Sets the startup handshake timeout in milliseconds.
    pub fn with_startup_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.startup_timeout_ms = timeout_ms;
        self
    }

    /// Sets the aggregate call timeout in milliseconds.
    pub fn with_call_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.call_timeout_ms = timeout_ms;
        self
    }

    /// Sets the orderly shutdown timeout in milliseconds.
    pub fn with_shutdown_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.shutdown_timeout_ms = timeout_ms;
        self
    }

    /// Sets all stream resource limits.
    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Mutable setter useful when options are assembled incrementally.
    pub fn set_transport(&mut self, transport: TransportKind) -> &mut Self {
        self.transport = transport;
        self
    }

    /// Mutable startup timeout setter.
    pub fn set_startup_timeout_ms(&mut self, timeout_ms: u64) -> &mut Self {
        self.startup_timeout_ms = timeout_ms;
        self
    }

    /// Mutable call timeout setter.
    pub fn set_call_timeout_ms(&mut self, timeout_ms: u64) -> &mut Self {
        self.call_timeout_ms = timeout_ms;
        self
    }

    /// Mutable shutdown timeout setter.
    pub fn set_shutdown_timeout_ms(&mut self, timeout_ms: u64) -> &mut Self {
        self.shutdown_timeout_ms = timeout_ms;
        self
    }

    /// Mutable limits setter.
    pub fn set_limits(&mut self, limits: ResourceLimits) -> &mut Self {
        self.limits = limits;
        self
    }
}
/// Installs the process-level limits before a native child executes (spec 24.4).
///
/// The worker calls this after constructing its restricted process command
/// (spec 16.6).
pub(crate) fn configure_command(command: &mut Command, options: &WorkerOptions) -> Result<(), String> {
    #[cfg(unix)]
    {
        unix::install_pre_exec(command, options.limits)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (command, options);
    }
    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::raw::c_int;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use super::ResourceLimits;

    #[repr(C)]
    struct RLimit {
        current: u64,
        maximum: u64,
    }

    #[link(name = "c")]
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn setrlimit(resource: c_int, limit: *const RLimit) -> c_int;
    }

    pub(super) fn install_pre_exec(command: &mut Command, limits: ResourceLimits) -> Result<(), String> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if limits.max_memory_bytes.is_some()
                || limits.max_cpu_time_seconds.is_some()
                || limits.max_processes.is_some()
                || limits.max_open_files.is_some()
            {
                // SAFETY: `pre_exec` runs in the child between fork and exec.
                // The closure captures only `Copy` data and calls the
                // async-signal-safe `setrlimit` syscall wrapper; it performs
                // no allocation, locking, or Rust I/O.
                #[allow(unsafe_code)]
                unsafe {
                    command.pre_exec(move || apply_limits(limits));
                }
            }
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (command, limits);
            Err("requested process limits are unavailable on this Unix target".to_owned())
        }
    }

    #[allow(unsafe_code)]
    fn apply_limits(limits: ResourceLimits) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            const RLIMIT_AS: c_int = 9;
            const RLIMIT_NPROC: c_int = 6;
            const RLIMIT_NOFILE: c_int = 7;
            const RLIMIT_CPU: c_int = 0;
            apply_one(RLIMIT_AS, limits.max_memory_bytes)?;
            apply_one(RLIMIT_CPU, limits.max_cpu_time_seconds)?;
            apply_one(RLIMIT_NPROC, limits.max_processes)?;
            apply_one(RLIMIT_NOFILE, limits.max_open_files)?;
        }
        #[cfg(target_os = "macos")]
        {
            const RLIMIT_AS: c_int = 5;
            const RLIMIT_NPROC: c_int = 7;
            const RLIMIT_NOFILE: c_int = 8;
            const RLIMIT_CPU: c_int = 0;
            apply_one(RLIMIT_AS, limits.max_memory_bytes)?;
            apply_one(RLIMIT_CPU, limits.max_cpu_time_seconds)?;
            apply_one(RLIMIT_NPROC, limits.max_processes)?;
            apply_one(RLIMIT_NOFILE, limits.max_open_files)?;
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    fn apply_one(resource: c_int, value: Option<u64>) -> io::Result<()> {
        if let Some(value) = value {
            let limit = RLimit {
                current: value,
                maximum: value,
            };
            // SAFETY: `limit` is a valid immutable pointer for this call, and
            // `setrlimit` is async-signal-safe in the post-fork child hook.
            if unsafe { setrlimit(resource, &limit) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}
