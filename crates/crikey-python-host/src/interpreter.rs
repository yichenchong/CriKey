//! Choosing the CPython that runs *modern* plugin code (spec 4.2, 14.11, 15.2).
//!
//! Like the Legacy Compatibility Layer, modern plugins never execute inside the
//! CriKey process: they run in a child process (spec 4.2), and the host first
//! has to decide *which* interpreter that child runs. The rule is fixed, total
//! and ordered:
//!
//! 1. the `CRIKEY_PYTHON` environment override,
//! 2. the interpreter named by [`RuntimeProfile::External`],
//! 3. the runtime bundled beside the running executable,
//! 4. `python3` on the search path.
//!
//! Rule 3 is what makes a shipped artefact self-contained (spec 14.11): an
//! installer that stages a relocatable CPython into [`BUNDLED_RUNTIME_DIR`]
//! beside the binary gets it chosen with no configuration at all, so the
//! product does not silently inherit whatever Python the machine happens to
//! have — and a machine with none still runs plugins that declare a
//! `requires-python`. It sits *below* the override and an explicitly named
//! interpreter because both of those are somebody's deliberate choice, and
//! *above* the search path because the search path is nobody's.
//!
//! Each rule is *decisive*. Once a rule names a candidate, that candidate is
//! the answer: if it cannot run, or does not satisfy the plugin's
//! `requires-python`, discovery fails — it never falls through to the next
//! rule. Falling through would run plugin code under an interpreter the plugin
//! declared it cannot run on, or one the operator did not choose, which is
//! worse than not starting at all (spec 15.2, §4). A bundled runtime is held
//! to exactly that standard: it is probed, version-gated and started with the
//! same isolation as any other candidate, so a broken staging is a loud
//! failure rather than a quiet downgrade to the system interpreter.
//!
//! # Which profile a plugin gets
//!
//! The rules above decide *whether* a named candidate may run plugin code; they
//! do not decide *which* interpreter a plugin should be offered. That is
//! [`RuntimeCatalog`]: it probes every interpreter on the search path once and
//! maps a declared `requires-python` onto the [`RuntimeProfile`] whose
//! interpreter satisfies it, so two plugins with incompatible requirements are
//! offered two different interpreters instead of both being gated against one.
//! The choice is then still passed through the ordered rules above, which is
//! why `CRIKEY_PYTHON` keeps winning over the mapping.
//!
//! # Why discovery takes the environment as a value
//!
//! [`discover_interpreter_in`] resolves against a [`DiscoveryEnvironment`] the
//! caller supplies, and [`discover_interpreter`] is exactly the same function
//! over the ambient process environment. The explicit form exists for
//! determinism: a process has one environment shared by every thread, so a
//! caller that had to mutate `CRIKEY_PYTHON` to select an interpreter would be
//! changing global state for unrelated work at the same time.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::worker::HostError;
use crate::RuntimeProfile;

/// Environment variable that overrides every other discovery rule (spec 14.11).
pub const ENV_PYTHON_OVERRIDE: &str = "CRIKEY_PYTHON";

/// Name of the directory a shipped artefact stages its own CPython into
/// (spec 14.11).
pub const BUNDLED_RUNTIME_DIR: &str = "python-runtime";

/// Where [`BUNDLED_RUNTIME_DIR`] is looked for, relative to the directory
/// holding the running executable, in order.
///
/// Relative to the binary rather than an absolute install prefix so the layout
/// survives being moved, copied or run from a portable directory — the same
/// reasoning, and the same shape, as the `modern-sdk` sibling that `sdk_root`
/// looks for. Two locations because two install shapes are real and neither
/// can be derived from the other: a self-contained directory (portable
/// archive, macOS `Contents/MacOS`, a `cargo build` output) puts the runtime
/// beside the binary, while a prefix install (`.deb`, `.rpm`, `/usr/local`)
/// cannot litter `bin/` and puts it under `lib/crikey/`.
const BUNDLED_RUNTIME_ROOTS: &[&str] = &[BUNDLED_RUNTIME_DIR, "../lib/crikey/python-runtime"];

/// Interpreter locations inside a staged runtime, in order, expressed relative
/// to it. Slash-separated because `Path::join` accepts `/` on every platform
/// this ships on, and a python-build-standalone tree keeps its interpreter at
/// `bin/python3` on Unix and at the prefix root on Windows.
#[cfg(windows)]
const BUNDLED_INTERPRETER_PATHS: &[&str] = &["python.exe", "python3.exe"];
#[cfg(not(windows))]
const BUNDLED_INTERPRETER_PATHS: &[&str] = &["bin/python3"];

/// The staged runtime's interpreter for an executable in `executable_dir`, if
/// one is there.
///
/// Existence and executability only: the version is never inferred from the
/// layout, because a staged runtime that reports the wrong version has to fail
/// the same probe every other candidate faces.
pub fn bundled_interpreter_beside(executable_dir: &Path) -> Option<PathBuf> {
    BUNDLED_RUNTIME_ROOTS
        .iter()
        .flat_map(|root| {
            let root = executable_dir.join(root);
            BUNDLED_INTERPRETER_PATHS
                .iter()
                .map(move |relative| root.join(relative))
        })
        .find(|candidate| is_executable_file(candidate))
}

/// Executable names tried, in order, when the search path is the deciding rule.
#[cfg(windows)]
const SEARCH_PATH_CANDIDATES: &[&str] = &["python3.exe", "python.exe"];
#[cfg(not(windows))]
const SEARCH_PATH_CANDIDATES: &[&str] = &["python3"];

/// How the interpreter is asked what it is.
///
/// Deliberately identical to the question a human would ask, so the answer can
/// be reproduced by hand from a shell when a diagnostic is disputed.
const VERSION_PROBE_ARGS: [&str; 2] = ["-c", "import sys; print('%d.%d.%d' % sys.version_info[:3])"];

/// How much of a failed probe's output is quoted back in the error.
///
/// A candidate that answers with megabytes is already broken; retaining all of
/// it to say so would make the diagnostic the second defect.
const PROBE_DIAGNOSTIC_BYTES: usize = 512;

/// How many lines of a probe's output are scanned for a version.
///
/// More than one because an interpreter may emit a deprecation notice first;
/// bounded because a candidate that has not answered by then is not going to.
const PROBE_SCAN_LINES: usize = 16;

/// Maximum time spent probing one explicitly selected interpreter.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace period for reaping a killed probe without blocking startup.
const PROBE_REAP_GRACE: Duration = Duration::from_millis(250);
/// Maximum time spent probing the whole search-path catalog.
const CATALOG_PROBE_BUDGET: Duration = Duration::from_secs(5);
/// A bounded PATH cannot force unbounded child-process probes.
const MAX_CATALOG_CANDIDATES: usize = 32;

/// Bound on captured probe output. The reader keeps draining after this limit
/// so a noisy candidate cannot deadlock on a full pipe.
const PROBE_OUTPUT_BYTES: usize = 16 * 1024;

/// A plugin's declared `requires-python` (spec 15.2, 19).
///
/// A comma-joined numeric release subset of `==`, `>=`, `>`, `<`, `<=`
/// clauses, e.g. `">=3.12"`. Pre-release and suffixed values are rejected
/// rather than guessed, because this host probes the stable
/// `major.minor.patch` release reported by `sys.version_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiresPython(pub String);

impl RequiresPython {
    /// Whether `version` satisfies every clause of this requirement.
    ///
    /// An empty requirement is satisfied by anything. An unparseable clause is
    /// treated as unmet: a requirement the host cannot understand must not be
    /// silently waved through onto plugin code.
    pub fn is_satisfied_by(&self, version: &PythonVersion) -> bool {
        let spec = self.0.trim();
        if spec.is_empty() {
            return true;
        }
        spec.split(',')
            .all(|clause| clause_is_satisfied(clause.trim(), version))
    }
}

/// Evaluates one PEP 440 subset clause (`">=3.12"`, `"<4"`, …) against a found
/// version. A bare version with no operator is read as `>=`, the permissive
/// reading a launcher wants for a floor.
fn clause_is_satisfied(clause: &str, version: &PythonVersion) -> bool {
    let (op, rest) = split_operator(clause);
    let Some(required) = parse_version(rest.trim()) else {
        return false;
    };
    match op {
        Operator::Eq => *version == required,
        Operator::Ge => *version >= required,
        Operator::Gt => *version > required,
        Operator::Le => *version <= required,
        Operator::Lt => *version < required,
    }
}

enum Operator {
    Eq,
    Ge,
    Gt,
    Le,
    Lt,
}

/// Splits a clause into its comparison operator and the version text.
///
/// Longer operators are matched first so `">="` is never mistaken for `">"`.
fn split_operator(clause: &str) -> (Operator, &str) {
    for (token, op) in [
        (">=", Operator::Ge),
        ("<=", Operator::Le),
        ("==", Operator::Eq),
        (">", Operator::Gt),
        ("<", Operator::Lt),
    ] {
        if let Some(rest) = clause.strip_prefix(token) {
            return (op, rest);
        }
    }
    // No operator: read a bare version as a floor.
    (Operator::Ge, clause)
}

/// A CPython `major.minor.patch` release.
///
/// The derived [`Ord`] *is* the version gate: fields are declared most
/// significant first, so a `requires-python` clause is decided by comparing
/// [`PythonVersion`]s and there is no second, hand-written comparison to
/// disagree with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PythonVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl PythonVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }
}

impl fmt::Display for PythonVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Which discovery rule selected an interpreter.
///
/// Reported rather than inferred: an operator debugging "why is my plugin
/// running under the wrong Python" needs to know which rule won, and the path
/// alone does not say (spec 26.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InterpreterSource {
    /// `CRIKEY_PYTHON`.
    EnvironmentOverride,
    /// [`RuntimeProfile::External`].
    RuntimeProfile,
    /// The runtime staged beside the running executable (spec 14.11).
    BundledRuntime,
    /// `python3` found on the search path.
    SearchPath,
}

impl InterpreterSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnvironmentOverride => "environment-override",
            Self::RuntimeProfile => "runtime-profile",
            Self::BundledRuntime => "bundled-runtime",
            Self::SearchPath => "search-path",
        }
    }
}

impl fmt::Display for InterpreterSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An interpreter that exists, runs, and satisfies the requirement it was
/// discovered against.
///
/// Only [`discover_interpreter_in`] constructs one, so holding an `Interpreter`
/// is proof the version was read from the executable rather than assumed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Interpreter {
    path: PathBuf,
    version: PythonVersion,
    source: InterpreterSource,
}

impl Interpreter {
    /// The executable, exactly as the winning rule named it.
    ///
    /// Never canonicalized: an operator who pointed `CRIKEY_PYTHON` at a
    /// symlink chose that symlink, and a diagnostic that quoted the resolved
    /// target back would not match what they configured.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn version(&self) -> PythonVersion {
        self.version
    }

    pub fn source(&self) -> InterpreterSource {
        self.source
    }
}

impl fmt::Display for Interpreter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}, via {})",
            self.path.display(),
            self.version,
            self.source
        )
    }
}

/// The inputs discovery is allowed to read.
///
/// Everything ambient that can decide the outcome lives here as data, which is
/// what makes [`discover_interpreter_in`] a pure function of its arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryEnvironment {
    python_override: Option<PathBuf>,
    /// Directory holding the running executable, which is where a shipped
    /// artefact's bundled runtime sits. `None` means "no bundled runtime is
    /// reachable", which is what an unconfigured test environment wants and
    /// what a host whose own path cannot be read has to assume.
    executable_dir: Option<PathBuf>,
    search_path: Vec<PathBuf>,
}

impl DiscoveryEnvironment {
    /// Nothing ambient at all: no override, no bundled runtime and no search
    /// path, so only [`RuntimeProfile::External`] can resolve against this.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads `CRIKEY_PYTHON`, the running executable's directory and `PATH`
    /// from the ambient process environment.
    pub fn from_process() -> Self {
        let python_override = std::env::var_os(ENV_PYTHON_OVERRIDE)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        // A host that cannot say where its own executable is simply has no
        // bundled runtime; that is a fall-through to the search path, not an
        // error, because the same host worked this way before bundling
        // existed.
        let executable_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let search_path = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();

        Self {
            python_override,
            executable_dir,
            search_path,
        }
    }

    pub fn with_override(mut self, path: impl AsRef<Path>) -> Self {
        self.python_override = Some(path.as_ref().to_path_buf());
        self
    }

    /// Treats `directory` as the one holding the running executable, and so as
    /// the parent of any bundled runtime.
    pub fn with_executable_dir(mut self, directory: impl AsRef<Path>) -> Self {
        self.executable_dir = Some(directory.as_ref().to_path_buf());
        self
    }

    pub fn with_search_path(mut self, directories: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.search_path = directories.into_iter().map(Into::into).collect();
        self
    }

    pub fn python_override(&self) -> Option<&Path> {
        self.python_override.as_deref()
    }

    pub fn executable_dir(&self) -> Option<&Path> {
        self.executable_dir.as_deref()
    }

    /// The bundled runtime's interpreter, when this build was shipped with one.
    pub fn bundled_interpreter(&self) -> Option<PathBuf> {
        self.executable_dir
            .as_deref()
            .and_then(bundled_interpreter_beside)
    }

    pub fn search_path(&self) -> &[PathBuf] {
        &self.search_path
    }

    /// First executable named by `SEARCH_PATH_CANDIDATES` on the search path.
    ///
    /// A directory or a non-executable file of the right name is skipped rather
    /// than chosen and then failed: a `PATH` scan looks for something it can
    /// run, and stopping at the first name match would let an unrelated file
    /// mask a usable interpreter further along.
    fn find_on_search_path(&self) -> Option<PathBuf> {
        self.search_path.iter().find_map(|directory| {
            SEARCH_PATH_CANDIDATES
                .iter()
                .map(|name| directory.join(name))
                .find(|candidate| is_executable_file(candidate))
        })
    }

    /// Every distinct interpreter the search path offers, in search order.
    ///
    /// Unlike [`Self::find_on_search_path`], which answers the single
    /// *decisive* `python3`, this enumerates the whole host so a plugin's
    /// `requires-python` can pick among the versions actually installed
    /// (spec 14.11). Duplicates are removed by resolved target, because
    /// `python3` is normally a symlink to `python3.<minor>` and probing both
    /// would spawn two processes to learn one version.
    fn enumerate_search_path(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut seen = BTreeSet::new();

        for directory in &self.search_path {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            // Sorted so the enumeration does not depend on directory order,
            // and so the short name (`python3`) is the one kept for a target
            // it shares with a versioned alias.
            let mut candidates: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(is_interpreter_name)
                        && is_executable_file(path)
                })
                .collect();
            candidates.sort();

            for candidate in candidates {
                let identity = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
                if seen.insert(identity) {
                    found.push(candidate);
                }
            }
        }

        found
    }
}

/// Whether `name` is a file name this host is willing to treat as a CPython.
///
/// `python3` and `python3.<minor>` only: a bare `python` on Unix is still a
/// Python 2 on some hosts and is not a name the decisive search-path rule
/// chooses either, so the catalog does not invent a candidate the rest of this
/// module would never pick. Windows ships the unsuffixed `python.exe` as the
/// normal name, so there it is included — matching [`SEARCH_PATH_CANDIDATES`].
fn is_interpreter_name(name: &str) -> bool {
    #[cfg(windows)]
    let lowered = name.to_ascii_lowercase();
    #[cfg(windows)]
    let name = lowered.as_str();

    #[cfg(windows)]
    let Some(stem) = name.strip_suffix(".exe") else {
        return false;
    };
    #[cfg(not(windows))]
    let stem = name;

    if cfg!(windows) && stem == "python" {
        return true;
    }
    let Some(rest) = stem.strip_prefix("python3") else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix('.')
            .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The interpreters this host offers, probed once (spec 14.11).
///
/// # Why this exists
///
/// A plugin declares a `requires-python`, not an interpreter. Without a catalog
/// the host can only offer one interpreter and the declaration degrades to a
/// yes/no gate on it, so a plugin needing 3.13 on a host whose `python3` is
/// 3.11 simply fails even though 3.13 is installed beside it — and two plugins
/// with incompatible requirements can never both run. Mapping a requirement to
/// a [`RuntimeProfile`] is what makes those two plugins land on two
/// interpreters and therefore, since the worker key carries the interpreter's
/// version, two separate processes (spec 14.11, 15.6).
///
/// # Why it is cached
///
/// The only way to know an interpreter's version is to run it. Mapping each
/// plugin independently would spawn one probe per candidate per plugin at
/// startup; the scan is therefore done once and every plugin is mapped from the
/// result. Nothing here is refreshed: an interpreter installed while the
/// launcher is running is picked up on the next start, which is the same
/// contract the search path itself has.
#[derive(Debug)]
pub struct RuntimeCatalog {
    /// Set when `CRIKEY_PYTHON` is present. The override is decisive in
    /// [`discover_interpreter_in`], so the mapping must not name a competing
    /// interpreter and the scan is skipped entirely.
    overridden: bool,
    /// The runtime shipped beside the executable, when it is present *and*
    /// runnable. Kept apart from `interpreters` because it maps to
    /// [`RuntimeProfile::Bundled`] rather than to a path, which is what keeps
    /// discovery reporting `bundled-runtime` as the winning rule.
    bundled: Option<Interpreter>,
    interpreters: Vec<Interpreter>,
}

impl RuntimeCatalog {
    /// Scans the ambient process environment.
    pub fn for_process() -> Self {
        Self::probe_in(&DiscoveryEnvironment::from_process())
    }

    /// Scans `environment`, probing every candidate it enumerates.
    ///
    /// A candidate that will not run, or answers with nothing recognisable as a
    /// version, is left out rather than reported: this is a *scan*, and unlike
    /// the decisive rules of [`discover_interpreter_in`] no plugin asked for
    /// this particular file. A broken `python3.9` beside a working `python3.13`
    /// must not stop the host from offering 3.13.
    ///
    /// The bundled runtime is probed as one more candidate, and dropped on the
    /// same terms. Dropping it does not hide the breakage: the mapping then
    /// names a host interpreter by path, so discovery reports `runtime-profile`
    /// rather than claiming a shipped runtime that does not work.
    pub fn probe_in(environment: &DiscoveryEnvironment) -> Self {
        if environment.python_override().is_some() {
            return Self {
                overridden: true,
                bundled: None,
                interpreters: Vec::new(),
            };
        }

        let deadline = Instant::now() + CATALOG_PROBE_BUDGET;
        let bundled = environment.bundled_interpreter().and_then(|path| {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            probe_with_timeout(&path, InterpreterSource::BundledRuntime, remaining).ok()
        });
        let interpreters = environment
            .enumerate_search_path()
            .into_iter()
            .take(MAX_CATALOG_CANDIDATES)
            .filter_map(|path| {
                let remaining = deadline.checked_duration_since(Instant::now())?;
                probe_with_timeout(&path, InterpreterSource::SearchPath, remaining).ok()
            })
            .collect();

        Self {
            overridden: false,
            bundled,
            interpreters,
        }
    }

    /// A catalog that offers exactly `interpreters`, bypassing the scan.
    ///
    /// Exists so the mapping rule can be exercised against version
    /// combinations no single host has installed.
    #[cfg(test)]
    fn of(interpreters: Vec<Interpreter>) -> Self {
        Self {
            overridden: false,
            bundled: None,
            interpreters,
        }
    }

    /// A catalog whose bundled runtime is `bundled`, bypassing the scan.
    #[cfg(test)]
    fn with_bundled(mut self, bundled: Interpreter) -> Self {
        self.bundled = Some(bundled);
        self
    }

    /// Maps a declared `requires-python` onto the profile whose interpreter
    /// satisfies it (spec 14.11).
    ///
    /// The newest satisfying interpreter wins, because a plugin that declares a
    /// floor wants the maintained runtime rather than the oldest one that still
    /// technically passes; ties are impossible and equal versions are broken by
    /// search-path order, so the answer is deterministic for a given host.
    ///
    /// The result is a profile, not an [`Interpreter`]: the caller still passes
    /// it through [`discover_interpreter_in`], which is the one place the
    /// `requires` gate and the `CRIKEY_PYTHON` override are applied. That keeps
    /// this a *choice* and leaves the *decision* where the ordered rules live.
    pub fn profile_for(&self, requires: &RequiresPython) -> Result<RuntimeProfile, HostError> {
        if self.overridden {
            // `Bundled` names no path, so discovery's first rule — the
            // override — stays decisive and the operator's choice is what runs
            // (and what the `requires` gate reports on).
            return Ok(RuntimeProfile::Bundled);
        }

        // A shipped artefact must not depend on a system-wide runtime
        // (spec 14.11), so a bundled interpreter that satisfies the declaration
        // wins even when the host happens to have a newer one installed. The
        // "newest wins" rule below only arbitrates between interpreters that
        // are all equally the machine's, not the product's.
        if self
            .bundled
            .as_ref()
            .is_some_and(|bundled| requires.is_satisfied_by(&bundled.version))
        {
            return Ok(RuntimeProfile::Bundled);
        }

        let chosen = self
            .interpreters
            .iter()
            .filter(|interpreter| requires.is_satisfied_by(&interpreter.version))
            .fold(None::<&Interpreter>, |best, candidate| match best {
                Some(best) if best.version >= candidate.version => Some(best),
                _ => Some(candidate),
            });

        match chosen {
            Some(interpreter) => Ok(RuntimeProfile::External(interpreter.path.clone())),
            None => Err(HostError::UnsatisfiedRequiresPython {
                required: requires.0.clone(),
                found: self.describe_found(),
            }),
        }
    }

    /// What the scan saw, for the failure message: an operator can only fix an
    /// unsatisfiable requirement if the diagnostic says which interpreters were
    /// considered and what versions they reported.
    fn describe_found(&self) -> String {
        let found: Vec<String> = self
            .bundled
            .iter()
            .chain(&self.interpreters)
            .map(Interpreter::to_string)
            .collect();
        if found.is_empty() {
            return format!("no {} on the search path", SEARCH_PATH_CANDIDATES.join(" or "));
        }
        found.join(", ")
    }
}

/// Resolves the interpreter for `profile` and `requires` against the ambient
/// process environment.
pub fn discover_interpreter(
    profile: &RuntimeProfile,
    requires: &RequiresPython,
) -> Result<Interpreter, HostError> {
    discover_interpreter_in(profile, requires, &DiscoveryEnvironment::from_process())
}

/// Resolves the interpreter for `profile` and `requires` against `environment`
/// (spec 14.11).
///
/// The order is `CRIKEY_PYTHON`, then [`RuntimeProfile::External`], then the
/// runtime bundled beside the executable, then the search path. Each rule is
/// *decisive*: once a rule names a candidate, a candidate that cannot be used
/// — because it will not run or because it does not satisfy `requires` — is
/// the answer, an error, and the remaining rules are never consulted.
pub fn discover_interpreter_in(
    profile: &RuntimeProfile,
    requires: &RequiresPython,
    environment: &DiscoveryEnvironment,
) -> Result<Interpreter, HostError> {
    if let Some(path) = environment.python_override() {
        return resolve(path, InterpreterSource::EnvironmentOverride, requires);
    }

    // `Bundled` names no path: it is the absence of an explicitly chosen
    // interpreter, so the remaining rules decide.
    if let RuntimeProfile::External(path) = profile {
        return resolve(path, InterpreterSource::RuntimeProfile, requires);
    }

    // Present *and* decisive: once a build ships its own runtime, silently
    // preferring the machine's reintroduces exactly the system-wide dependency
    // bundling exists to remove, so a staged runtime that will not run is a
    // failure rather than an invisible downgrade.
    if let Some(path) = environment.bundled_interpreter() {
        return resolve(&path, InterpreterSource::BundledRuntime, requires);
    }

    match environment.find_on_search_path() {
        Some(path) => resolve(&path, InterpreterSource::SearchPath, requires),
        None => Err(HostError::Interpreter(format!(
            "no {} on the search path, no {BUNDLED_RUNTIME_DIR} runtime beside the executable, \
             and neither {ENV_PYTHON_OVERRIDE} nor an external runtime profile named one",
            SEARCH_PATH_CANDIDATES.join(" or "),
        ))),
    }
}

/// Probes the candidate named by a decisive rule and applies the `requires`
/// gate.
///
/// The gate is applied here, not by the caller, so that a candidate failing
/// `requires` is a named error at the point the rule chose it — never a fall-
/// through to the next rule (spec 15.2).
fn resolve(
    path: &Path,
    source: InterpreterSource,
    requires: &RequiresPython,
) -> Result<Interpreter, HostError> {
    let interpreter = probe(path, source)?;
    if !requires.is_satisfied_by(&interpreter.version) {
        return Err(HostError::UnsatisfiedRequiresPython {
            required: requires.0.clone(),
            found: interpreter.version.to_string(),
        });
    }
    Ok(interpreter)
}

/// Executes `path` and reads the version it reports.
fn probe(path: &Path, source: InterpreterSource) -> Result<Interpreter, HostError> {
    probe_with_timeout(path, source, PROBE_TIMEOUT)
}

fn probe_with_timeout(
    path: &Path,
    source: InterpreterSource,
    timeout: Duration,
) -> Result<Interpreter, HostError> {
    let mut command = Command::new(path);
    sanitize_python_environment(&mut command);
    command
        .args(VERSION_PROBE_ARGS)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command.spawn().map_err(|error| {
        HostError::Interpreter(format!(
            "the {source} interpreter candidate {} could not be executed: {error}",
            path.display()
        ))
    })?;
    let stdout = child.stdout.take().expect("probe stdout is piped");
    let stderr = child.stderr.take().expect("probe stderr is piped");
    let stdout_capture = Arc::new(Mutex::new(ProbeCapture::default()));
    let stderr_capture = Arc::new(Mutex::new(ProbeCapture::default()));
    let stdout_thread = spawn_probe_drain(stdout, Arc::clone(&stdout_capture));
    let stderr_thread = spawn_probe_drain(stderr, Arc::clone(&stderr_capture));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(None) | Err(_) => {
                hard_kill_probe(child.id(), &mut child);
                if let Some(status) = wait_probe_bounded(&mut child, PROBE_REAP_GRACE) {
                    let _ = status;
                } else {
                    reap_probe_in_background(child);
                }
                finish_probe_drain(stdout_thread, &stdout_capture);
                finish_probe_drain(stderr_thread, &stderr_capture);
                return Err(HostError::Interpreter(format!(
                    "the {source} interpreter candidate {} did not answer its version probe \
                     within {:?} and was stopped",
                    path.display(),
                    timeout
                )));
            }
        }
    };
    finish_probe_drain(stdout_thread, &stdout_capture);
    finish_probe_drain(stderr_thread, &stderr_capture);
    let stdout = snapshot_probe(&stdout_capture);
    let stderr = snapshot_probe(&stderr_capture);

    if !status.success() {
        return Err(HostError::Interpreter(format!(
            "the {source} interpreter candidate {} exited with {} (stdout: {}; stderr: {})",
            path.display(),
            format_exit(status),
            excerpt(&stdout),
            excerpt(&stderr),
        )));
    }

    let Some(found) = parse_reported_version(&stdout) else {
        return Err(HostError::Interpreter(format!(
            "the {source} interpreter candidate {} did not report a version (stdout: {}; \
             stderr: {})",
            path.display(),
            excerpt(&stdout),
            excerpt(&stderr),
        )));
    };

    Ok(Interpreter {
        path: path.to_path_buf(),
        version: found,
        source,
    })
}

#[derive(Debug, Default)]
struct ProbeCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn spawn_probe_drain<R: Read + Send + 'static>(
    mut reader: R,
    capture: Arc<Mutex<ProbeCapture>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            let count = match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            let mut output = capture.lock().unwrap_or_else(|error| error.into_inner());
            let room = PROBE_OUTPUT_BYTES.saturating_sub(output.bytes.len());
            if room > 0 {
                output.bytes.extend_from_slice(&buffer[..count.min(room)]);
            }
            if count > room {
                output.truncated = true;
            }
        }
    })
}

fn finish_probe_drain(handle: JoinHandle<()>, capture: &Arc<Mutex<ProbeCapture>>) {
    let deadline = Instant::now() + PROBE_REAP_GRACE;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    if handle.is_finished() {
        let _ = handle.join();
    } else {
        let _ = capture;
    }
}

fn snapshot_probe(capture: &Arc<Mutex<ProbeCapture>>) -> Vec<u8> {
    let output = capture.lock().unwrap_or_else(|error| error.into_inner());
    output.bytes.clone()
}

fn wait_probe_bounded(child: &mut Child, budget: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(1)),
            Ok(None) | Err(_) => return None,
        }
    }
}

fn reap_probe_in_background(mut child: Child) {
    let _ = thread::spawn(move || loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => thread::sleep(Duration::from_millis(1)),
        }
    });
}

fn format_exit(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => code.to_string(),
        None => "signalled".to_owned(),
    }
}

/// Removes every interpreter-controlled inherited variable before a Python
/// process is started. The worker adds back only the explicit values it owns.
pub(crate) fn sanitize_python_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().to_ascii_uppercase().starts_with("PYTHON") {
            command.env_remove(name);
        }
    }
}

fn hard_kill_probe(process_id: u32, child: &mut Child) {
    #[cfg(unix)]
    kill_probe_process_group(process_id);
    #[cfg(not(unix))]
    let _ = process_id;
    let _ = child.kill();
}

#[cfg(unix)]
fn kill_probe_process_group(process_id: u32) {
    // `killpg(0)` would signal this launcher's own process group; the probe pid
    // is always a live `Child::id()`, so treat "myself"/"init" as impossible.
    if process_id <= 1 {
        return;
    }
    extern "C" {
        fn killpg(pgrp: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    #[allow(
        unsafe_code,
        reason = "no safe std API kills a process group; args are a validated pgid and a constant signal"
    )]
    unsafe {
        let _ = killpg(process_id as i32, SIGKILL);
    }
}

/// First line of `stdout` that is a bare `major[.minor[.patch]]`.
///
/// Strict on purpose: a line that is nearly a version is rejected rather than
/// partially parsed, because a misread version silently downgrades the gate
/// that keeps plugin code off an interpreter it cannot run on.
fn parse_reported_version(stdout: &[u8]) -> Option<PythonVersion> {
    let text = std::str::from_utf8(stdout).ok()?;
    text.lines()
        .take(PROBE_SCAN_LINES)
        .find_map(|line| parse_version(line.trim()))
}

fn parse_version(text: &str) -> Option<PythonVersion> {
    if text.is_empty() {
        return None;
    }

    let mut components = text.split('.');
    let major = parse_component(components.next()?)?;
    let minor = components.next().map_or(Some(0), parse_component)?;
    let patch = components.next().map_or(Some(0), parse_component)?;
    if components.next().is_some() {
        return None;
    }

    Some(PythonVersion::new(major, minor, patch))
}

fn parse_component(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A bounded, single-line rendering of a failed probe's diagnostic output.
fn excerpt(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(PROBE_DIAGNOSTIC_BYTES)]);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        "no output".to_owned()
    } else {
        collapsed
    }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the Unix environment-scrubbing test serialises on a lock.
    #[cfg(unix)]
    use std::sync::LazyLock;

    #[test]
    fn numeric_components_are_compared_as_numbers() {
        assert!(parse_version("3.10").expect("3.10 parses") > parse_version("3.9").expect("3.9 parses"));
        assert_eq!(parse_version("3.10"), Some(PythonVersion::new(3, 10, 0)));
    }

    #[test]
    fn pre_release_suffixed_and_unexpected_reports_are_rejected() {
        for report in [
            b"3.12.0rc1\n".as_slice(),
            b"3.12.0+vendor\n".as_slice(),
            b"+3.12.0\n".as_slice(),
            b"Python 3.12.0\n".as_slice(),
            b"3.12.0.1\n".as_slice(),
        ] {
            assert_eq!(
                parse_reported_version(report),
                None,
                "unexpected report {report:?}"
            );
        }
        assert_eq!(
            parse_reported_version(b"notice before answer\n3.12.0\n"),
            Some(PythonVersion::new(3, 12, 0))
        );
    }
    #[test]
    fn invalid_utf8_in_version_probe_output_is_rejected() {
        assert_eq!(parse_reported_version(b"notice \xff\n3.12.0\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn hostile_python_environment_is_removed_before_starting_a_child() {
        static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        let _guard = ENV_LOCK
            .lock()
            .expect("Python environment test lock is not poisoned");
        let old_home = std::env::var_os("PYTHONHOME");
        let old_path = std::env::var_os("PYTHONPATH");
        std::env::set_var("PYTHONHOME", "/does/not/exist");
        std::env::set_var("PYTHONPATH", "/hostile/path");

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("test -z \"$PYTHONHOME\" && test -z \"$PYTHONPATH\"");
        sanitize_python_environment(&mut command);
        let status = command.status().expect("shell probe starts");

        match old_home {
            Some(value) => std::env::set_var("PYTHONHOME", value),
            None => std::env::remove_var("PYTHONHOME"),
        }
        match old_path {
            Some(value) => std::env::set_var("PYTHONPATH", value),
            None => std::env::remove_var("PYTHONPATH"),
        }
        assert!(
            status.success(),
            "sanitizing Python variables leaves no hostile startup values"
        );
    }

    /// A catalog entry with a known version, so the mapping rule can be tested
    /// against version combinations no single host has installed.
    fn found(path: &str, major: u32, minor: u32, patch: u32) -> Interpreter {
        Interpreter {
            path: PathBuf::from(path),
            version: PythonVersion::new(major, minor, patch),
            source: InterpreterSource::SearchPath,
        }
    }

    fn host_with_three_versions() -> RuntimeCatalog {
        RuntimeCatalog::of(vec![
            found("/usr/bin/python3", 3, 11, 9),
            found("/usr/bin/python3.12", 3, 12, 7),
            found("/usr/bin/python3.13", 3, 13, 1),
        ])
    }

    /// A staged runtime entry, so the mapping rule can be tested without a
    /// real installation tree beside the test binary.
    fn staged(path: &str, major: u32, minor: u32, patch: u32) -> Interpreter {
        Interpreter {
            path: PathBuf::from(path),
            version: PythonVersion::new(major, minor, patch),
            source: InterpreterSource::BundledRuntime,
        }
    }

    #[test]
    fn a_satisfying_bundled_runtime_is_mapped_ahead_of_a_newer_interpreter_on_the_search_path() {
        // 3.13.1 is installed on this host and 3.12.4 is what the artefact
        // ships. The shipped one wins: bundling is about not depending on the
        // machine, and "newest wins" only arbitrates between the machine's own.
        let profile = host_with_three_versions()
            .with_bundled(staged("/opt/crikey/python-runtime/bin/python3", 3, 12, 4))
            .profile_for(&RequiresPython(">=3.12".to_owned()))
            .expect("the bundled 3.12.4 satisfies >=3.12");

        assert_eq!(
            profile,
            RuntimeProfile::Bundled,
            "a satisfying shipped runtime is mapped to the bundled profile, not to a host path"
        );
    }

    #[test]
    fn a_bundled_runtime_that_cannot_satisfy_the_requirement_does_not_hide_one_that_can() {
        // Honest degradation: the artefact ships 3.10, the plugin needs 3.13,
        // and the host has 3.13. Refusing here would fail a plugin that can
        // demonstrably run, so the mapping names the host interpreter — and
        // because the profile then names a path, discovery reports it as the
        // runtime-profile rule rather than pretending it was bundled.
        let profile = host_with_three_versions()
            .with_bundled(staged("/opt/crikey/python-runtime/bin/python3", 3, 10, 3))
            .profile_for(&RequiresPython(">=3.13".to_owned()))
            .expect("the host's 3.13.1 satisfies >=3.13 even though the bundled runtime does not");

        assert_eq!(
            profile,
            RuntimeProfile::External(PathBuf::from("/usr/bin/python3.13"))
        );
    }

    #[test]
    fn an_unsatisfiable_requirement_quotes_the_bundled_runtime_among_what_was_found() {
        let error = RuntimeCatalog::of(Vec::new())
            .with_bundled(staged("/opt/crikey/python-runtime/bin/python3", 3, 10, 3))
            .profile_for(&RequiresPython(">=3.13".to_owned()))
            .expect_err("neither the bundled runtime nor the empty host satisfies >=3.13");
        let message = error.to_string();

        assert!(
            message.contains("python-runtime") && message.contains("3.10.3"),
            "an operator cannot fix the staging unless the diagnostic names it: {message}"
        );
    }

    #[test]
    fn the_newest_interpreter_satisfying_the_requirement_is_the_one_mapped() {
        let profile = host_with_three_versions()
            .profile_for(&RequiresPython(">=3.12".to_owned()))
            .expect("3.12 and 3.13 both satisfy >=3.12");
        assert_eq!(
            profile,
            RuntimeProfile::External(PathBuf::from("/usr/bin/python3.13"))
        );
    }

    #[test]
    fn two_incompatible_requirements_map_to_two_different_interpreters() {
        let host = host_with_three_versions();
        let newer = host
            .profile_for(&RequiresPython(">=3.13".to_owned()))
            .expect("3.13.1 satisfies >=3.13");
        let older = host
            .profile_for(&RequiresPython("<3.12".to_owned()))
            .expect("3.11.9 satisfies <3.12");

        assert_eq!(
            newer,
            RuntimeProfile::External(PathBuf::from("/usr/bin/python3.13"))
        );
        assert_eq!(older, RuntimeProfile::External(PathBuf::from("/usr/bin/python3")));
        // The point of the mapping: incompatible requirements cannot share an
        // interpreter, hence cannot share a worker process.
        assert_ne!(
            newer, older,
            "incompatible requirements must not map to one interpreter"
        );
    }

    #[test]
    fn an_unsatisfiable_requirement_names_the_requirement_and_every_version_found() {
        let error = host_with_three_versions()
            .profile_for(&RequiresPython("==3.9.9".to_owned()))
            .expect_err("no 3.9 is installed on this fixture host");
        let message = error.to_string();

        assert!(
            message.contains("==3.9.9"),
            "the requirement must be quoted: {message}"
        );
        for version in ["3.11.9", "3.12.7", "3.13.1"] {
            assert!(
                message.contains(version),
                "the versions actually found must be quoted: {message}"
            );
        }
    }

    #[test]
    fn an_unsatisfiable_requirement_on_a_host_with_no_interpreter_says_so() {
        let error = RuntimeCatalog::of(Vec::new())
            .profile_for(&RequiresPython(">=3.12".to_owned()))
            .expect_err("an empty host satisfies nothing");
        assert!(
            error.to_string().contains("on the search path"),
            "an empty catalog must say the search path held nothing: {error}"
        );
    }

    #[test]
    fn an_environment_override_maps_every_requirement_to_the_bundled_profile() {
        let catalog = RuntimeCatalog::probe_in(
            &DiscoveryEnvironment::empty()
                .with_override("/opt/operator/python3")
                .with_search_path(["/usr/bin"]),
        );

        assert!(
            catalog.interpreters.is_empty(),
            "an override makes the scan pointless, so nothing is probed"
        );
        // `Bundled` names no path, so discovery's first rule — the override —
        // stays decisive and the operator's interpreter is what runs.
        assert_eq!(
            catalog
                .profile_for(&RequiresPython(">=3.13".to_owned()))
                .expect("an override never fails the mapping"),
            RuntimeProfile::Bundled
        );
    }

    #[test]
    fn only_python3_file_names_are_treated_as_interpreters() {
        for name in ["python3", "python3.9", "python3.13"] {
            assert!(is_interpreter_name(name), "{name} is an interpreter name");
        }
        for name in [
            "python",
            "python3.",
            "python3.x",
            "python3x",
            "pythonw3",
            "python2.7",
        ] {
            assert_eq!(
                is_interpreter_name(name),
                cfg!(windows) && name == "python",
                "{name} must not be picked up off the search path"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_search_path_scan_skips_non_executables_and_collapses_aliases() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("crikey-catalog-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture directory is creatable");

        let real = root.join("python3.13");
        std::fs::write(&real, "#!/bin/sh\nexit 0\n").expect("fixture interpreter is writable");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755))
            .expect("fixture interpreter is executable");
        // The usual layout: `python3` is an alias for the versioned binary.
        std::os::unix::fs::symlink("python3.13", root.join("python3")).expect("alias is creatable");
        // Neither of these is a runnable interpreter.
        std::fs::write(root.join("python3.9"), "not executable").expect("decoy is writable");
        std::fs::create_dir_all(root.join("python3.8")).expect("decoy directory is creatable");

        let found = DiscoveryEnvironment::empty()
            .with_search_path([root.clone()])
            .enumerate_search_path();
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(
            found,
            vec![root.join("python3")],
            "one target is one candidate, and a non-executable of the right name is not one"
        );
    }
}
