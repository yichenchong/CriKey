//! Kernel-enforced confinement for supervised plugin processes (spec 20.2, 24.4).
//!
//! # What this buys, exactly
//!
//! Every runtime that executes third-party code — native plugins, the WASM and
//! C-ABI hosts, modern Python workers, legacy Keypirinha workers — runs in a
//! child process the supervisor owns. Process isolation already contains a
//! crash. It contains nothing else: a plugin that runs at all runs with the
//! user's full authority and can rewrite the user's files, other plugins'
//! code, or the launcher's own installation.
//!
//! On Linux this crate closes the *write* half of that hole with Landlock:
//! the child may create, modify, rename, truncate or delete files only beneath
//! the directories the host names for it. The kernel enforces it, the plugin
//! cannot opt out, and `no_new_privs` is set alongside so a set-uid binary
//! cannot be used to step around it.
//!
//! # What this deliberately does NOT buy
//!
//! * **No confidentiality.** Read and execute rights are not restricted at
//!   all. A confined plugin can still read every file the user can read. That
//!   is a deliberate choice, not an oversight: a read allowlist that is wrong
//!   breaks the plugin's own interpreter, and a read allowlist that is right
//!   for CPython is close to "everything" anyway. Anything claiming otherwise
//!   in a UI would be a lie.
//! * **No syscall filtering.** There is no seccomp policy here. A plugin may
//!   call anything its runtime calls.
//! * **TCP only, and only when asked.** [`SandboxPolicy::deny_tcp`] uses
//!   Landlock's network rules, which govern `bind(2)` and `connect(2)` for
//!   TCP and nothing else. UDP, Unix sockets, netlink and an already-connected
//!   inherited socket are unaffected. On a kernel older than Landlock ABI v4
//!   the request is reported [`Enforcement::Unavailable`], never silently
//!   dropped.
//! * **Linux only.** Windows and macOS report [`Enforcement::Unavailable`]
//!   with the reason. The Windows job object the native host already installs
//!   is a resource limit, not a sandbox, and this crate does not relabel it as
//!   one.
//!
//! # Operator override
//!
//! `CRIKEY_PLUGIN_SANDBOX=off` disables enforcement for the whole process,
//! matching the `CRIKEY_PYTHON` precedent for host-level escape hatches. Any
//! other value is treated as `enforce` and reported as unrecognised, so a typo
//! fails closed instead of quietly removing the confinement.

use std::fmt;
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "linux")]
mod landlock;

/// Whether this process should confine the plugin children it spawns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Install the confinement the policy describes.
    Enforce,
    /// Spawn children unconfined because the operator asked for it.
    Off,
}

/// The environment variable an operator sets to disable confinement.
pub const ENV_SANDBOX_MODE: &str = "CRIKEY_PLUGIN_SANDBOX";

impl SandboxMode {
    /// Reads [`ENV_SANDBOX_MODE`] from this process.
    pub fn from_process() -> Self {
        Self::from_value(std::env::var(ENV_SANDBOX_MODE).ok().as_deref())
    }

    /// Resolves a value as if it had come from the environment.
    ///
    /// An unrecognised value is [`SandboxMode::Enforce`]: the failure mode of
    /// a misspelled `off` must be a confined plugin, not an unconfined one.
    /// The spelling is still surfaced through
    /// [`SandboxPolicy::unrecognised_mode`] so the operator learns their
    /// override did nothing.
    pub fn from_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("off") => Self::Off,
            _ => Self::Enforce,
        }
    }
}

/// What one confinement mechanism actually does to a child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enforcement {
    /// The caller did not ask for this restriction.
    NotRequested,
    /// The kernel enforces this restriction on the child.
    Enforced {
        /// The mechanism doing the enforcing, for diagnostics.
        mechanism: &'static str,
        /// The Landlock ABI level the policy was built against.
        abi: u32,
    },
    /// This restriction was asked for and cannot be provided here.
    Unavailable(String),
}

impl Enforcement {
    /// Whether a child really is restricted by this mechanism.
    pub fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforced { .. })
    }
}

impl fmt::Display for Enforcement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRequested => formatter.write_str("not requested"),
            Self::Enforced { mechanism, abi } => {
                write!(formatter, "enforced by {mechanism} (ABI {abi})")
            }
            Self::Unavailable(reason) => write!(formatter, "unavailable: {reason}"),
        }
    }
}

/// Honest per-mechanism report for one prepared sandbox (spec 20.2).
///
/// Every field is what the *child* will actually be subject to, decided before
/// the child exists. Nothing here is a plan or an intention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxReport {
    /// Whether writes outside the allowlist are refused by the kernel.
    pub filesystem_write: Enforcement,
    /// Whether TCP `bind` and `connect` are refused by the kernel.
    pub tcp_network: Enforcement,
    /// Paths the child may write beneath, as the kernel was told them.
    pub writable: Vec<PathBuf>,
    /// Paths the policy named that do not exist and were therefore not granted.
    ///
    /// These are not failures: a host names the directories a plugin *may*
    /// use, and one that has never been created is simply not there yet. It is
    /// reported because "the plugin cannot write its cache" and "the cache
    /// directory was missing at spawn" look identical from inside the plugin.
    pub skipped: Vec<PathBuf>,
}

impl SandboxReport {
    /// A report for a child that is not confined at all, with the reason.
    fn unconfined(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            filesystem_write: Enforcement::Unavailable(reason.clone()),
            tcp_network: Enforcement::Unavailable(reason),
            writable: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// A report for a child nobody asked to confine.
    fn not_requested() -> Self {
        Self {
            filesystem_write: Enforcement::NotRequested,
            tcp_network: Enforcement::NotRequested,
            writable: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// Reads are never restricted; say so where a reader will see it.
    pub const READS: &'static str = "reads and execution are not restricted";
}

impl fmt::Display for SandboxReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "filesystem writes: {}; tcp network: {}; {}",
            self.filesystem_write,
            self.tcp_network,
            Self::READS
        )
    }
}

/// What one supervised child is allowed to do to the filesystem and network.
///
/// The policy is a *write* allowlist. Everything not named is read-only to the
/// child; nothing is made unreadable.
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    writable: Vec<PathBuf>,
    deny_tcp: bool,
    mode: Option<SandboxMode>,
    unrecognised_mode: Option<String>,
}

impl SandboxPolicy {
    /// A policy that confines a child to the system temporary directory.
    ///
    /// Every runtime needs somewhere scratch: CPython writes temporary files
    /// through `tempfile`, and a plugin that cannot create one fails in ways
    /// that look nothing like a permission problem. The temporary directory is
    /// the one location whose whole contract is that its contents are
    /// disposable, so granting it costs no integrity worth having.
    pub fn scratch_only() -> Self {
        Self {
            writable: vec![std::env::temp_dir()],
            deny_tcp: false,
            mode: None,
            unrecognised_mode: None,
        }
    }

    /// Adds a directory (or file) the child may write beneath.
    ///
    /// A path that does not exist when the sandbox is prepared is reported in
    /// [`SandboxReport::skipped`] rather than failing the spawn.
    pub fn allow_write(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if !self.writable.iter().any(|existing| existing == &path) {
            self.writable.push(path);
        }
        self
    }

    /// Adds several writable paths, skipping the ones already present.
    pub fn allow_writes<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        for path in paths {
            self = self.allow_write(path);
        }
        self
    }

    /// Refuses TCP `bind` and `connect` for the child.
    ///
    /// Callers pass the manifest's network grant here: a plugin that declared
    /// no network access has no reason to open a socket, and Landlock can say
    /// so to the kernel. UDP and Unix sockets are outside what Landlock
    /// governs and stay reachable — see the module documentation.
    pub fn deny_tcp(mut self, deny: bool) -> Self {
        self.deny_tcp = deny;
        self
    }

    /// Overrides the process-wide mode, for tests and for explicit callers.
    pub fn with_mode(mut self, mode: SandboxMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// The unrecognised `CRIKEY_PLUGIN_SANDBOX` spelling, if there was one.
    pub fn unrecognised_mode(&self) -> Option<&str> {
        self.unrecognised_mode.as_deref()
    }

    /// The mode this policy resolves to.
    fn mode(&self) -> SandboxMode {
        self.mode.unwrap_or_else(SandboxMode::from_process)
    }

    /// The writable set, in the order the host named it.
    pub fn writable(&self) -> &[PathBuf] {
        &self.writable
    }

    /// Whether the policy asks for TCP to be refused.
    pub fn denies_tcp(&self) -> bool {
        self.deny_tcp
    }

    /// Builds the confinement, or the honest reason there is none.
    pub fn prepare(&self) -> PreparedSandbox {
        if self.mode() == SandboxMode::Off {
            return PreparedSandbox {
                report: SandboxReport::unconfined(format!(
                    "{ENV_SANDBOX_MODE}=off disabled plugin confinement for this process"
                )),
                #[cfg(target_os = "linux")]
                ruleset: None,
            };
        }
        if self.writable.is_empty() && !self.deny_tcp {
            return PreparedSandbox {
                report: SandboxReport::not_requested(),
                #[cfg(target_os = "linux")]
                ruleset: None,
            };
        }
        #[cfg(target_os = "linux")]
        {
            landlock::prepare(self)
        }
        #[cfg(not(target_os = "linux"))]
        {
            PreparedSandbox {
                report: SandboxReport::unconfined(
                    "this platform has no per-process filesystem confinement CriKey can install",
                ),
            }
        }
    }
}

/// A confinement built in the parent and ready to install in a child.
///
/// Preparation happens before `fork` on purpose: building the rule set opens
/// directories and allocates, neither of which is safe between `fork` and
/// `exec`. What runs in the child is two syscalls on an already-built
/// descriptor.
#[derive(Debug)]
pub struct PreparedSandbox {
    report: SandboxReport,
    #[cfg(target_os = "linux")]
    ruleset: Option<std::os::fd::OwnedFd>,
}

impl PreparedSandbox {
    /// What the child will actually be subject to.
    pub fn report(&self) -> &SandboxReport {
        &self.report
    }

    /// Whether anything at all will be enforced on the child.
    pub fn is_active(&self) -> bool {
        self.report.filesystem_write.is_enforced() || self.report.tcp_network.is_enforced()
    }

    /// Arranges for `command`'s child to confine itself before `exec`.
    ///
    /// Safe to call on an unconfined sandbox: it does nothing, which is what
    /// makes the call site free of platform conditionals.
    pub fn install(&self, command: &mut Command) {
        #[cfg(target_os = "linux")]
        landlock::install(self.ruleset.as_ref(), command);
        #[cfg(not(target_os = "linux"))]
        let _ = command;
    }
}

/// Files under `/dev` a confined child may write to.
///
/// Redirecting a child's output to `/dev/null` is ordinary, and a plugin that
/// cannot open it fails in a way that looks like a bug in CriKey. These are
/// character devices with no integrity to protect; `/dev` itself stays
/// unwritable, so no plugin can add a device node.
pub const WRITABLE_DEVICE_FILES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];

/// Directories under `/dev` a confined child may write beneath.
///
/// POSIX shared memory is how CPython's `multiprocessing` allocates, so a
/// plugin using the standard library would otherwise fail here.
pub const WRITABLE_DEVICE_DIRECTORIES: &[&str] = &["/dev/shm"];

/// The paths every confined child gets, whatever runtime it is.
pub fn baseline_writable_paths() -> Vec<PathBuf> {
    let mut paths = vec![std::env::temp_dir()];
    paths.extend(WRITABLE_DEVICE_DIRECTORIES.iter().map(PathBuf::from));
    paths.extend(WRITABLE_DEVICE_FILES.iter().map(PathBuf::from));
    paths
}

/// A policy for one plugin runtime: scratch space plus what the host handed it.
///
/// `writable` is the set of directories the host itself told the plugin about
/// — its cache, its data directory, its package cache. Anything else the
/// plugin knows about it found on its own, and a plugin writing to a location
/// nobody gave it is precisely what this refuses.
pub fn plugin_policy<I, P>(writable: I, deny_tcp: bool) -> SandboxPolicy
where
    I: IntoIterator<Item = P>,
    P: Into<PathBuf>,
{
    SandboxPolicy::default()
        .allow_writes(baseline_writable_paths())
        .allow_writes(writable)
        .deny_tcp(deny_tcp)
}

/// The system temporary directory, named once so a caller can exclude it.
pub fn scratch_directory() -> PathBuf {
    std::env::temp_dir()
}
