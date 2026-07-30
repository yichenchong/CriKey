//! Choosing the CPython that runs *modern* plugin code (spec 4.2, 14.11, 15.2).
//!
//! Like the Legacy Compatibility Layer, modern plugins never execute inside the
//! CriKey process: they run in a child process (spec 4.2), and the host first
//! has to decide *which* interpreter that child runs. The rule is fixed, total
//! and ordered:
//!
//! 1. the `CRIKEY_PYTHON` environment override,
//! 2. the interpreter named by [`RuntimeProfile::External`],
//! 3. `python3` on the search path.
//!
//! Each rule is *decisive*. Once a rule names a candidate, that candidate is
//! the answer: if it cannot run, or does not satisfy the plugin's
//! `requires-python`, discovery fails — it never falls through to the next
//! rule. Falling through would run plugin code under an interpreter the plugin
//! declared it cannot run on, or one the operator did not choose, which is
//! worse than not starting at all (spec 15.2, §4).
//!
//! # Why discovery takes the environment as a value
//!
//! [`discover_interpreter_in`] resolves against a [`DiscoveryEnvironment`] the
//! caller supplies, and [`discover_interpreter`] is exactly the same function
//! over the ambient process environment. The explicit form exists for
//! determinism: a process has one environment shared by every thread, so a
//! caller that had to mutate `CRIKEY_PYTHON` to select an interpreter would be
//! changing global state for unrelated work at the same time.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::worker::HostError;
use crate::RuntimeProfile;

/// Environment variable that overrides every other discovery rule (spec 14.11).
pub const ENV_PYTHON_OVERRIDE: &str = "CRIKEY_PYTHON";

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

/// A plugin's declared `requires-python` (spec 15.2, 19).
///
/// A comma-joined PEP 440 subset of `==`, `>=`, `>`, `<`, `<=` clauses, e.g.
/// `">=3.12"`. The gate is applied through [`PythonVersion`]'s derived
/// ordering, so there is no second, hand-written comparison to disagree with
/// the one that decides support.
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
    /// `python3` found on the search path.
    SearchPath,
}

impl InterpreterSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnvironmentOverride => "environment-override",
            Self::RuntimeProfile => "runtime-profile",
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
    search_path: Vec<PathBuf>,
}

impl DiscoveryEnvironment {
    /// No override and no search path: only [`RuntimeProfile::External`] can
    /// resolve against this.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads `CRIKEY_PYTHON` and `PATH` from the ambient process environment.
    pub fn from_process() -> Self {
        let python_override = std::env::var_os(ENV_PYTHON_OVERRIDE)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let search_path = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();

        Self {
            python_override,
            search_path,
        }
    }

    pub fn with_override(mut self, path: impl AsRef<Path>) -> Self {
        self.python_override = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn with_search_path(mut self, directories: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.search_path = directories.into_iter().map(Into::into).collect();
        self
    }

    pub fn python_override(&self) -> Option<&Path> {
        self.python_override.as_deref()
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
/// search path. Each rule is *decisive*: once a rule names a candidate, a
/// candidate that cannot be used — because it will not run or because it does
/// not satisfy `requires` — is the answer, an error, and the remaining rules
/// are never consulted.
pub fn discover_interpreter_in(
    profile: &RuntimeProfile,
    requires: &RequiresPython,
    environment: &DiscoveryEnvironment,
) -> Result<Interpreter, HostError> {
    if let Some(path) = environment.python_override() {
        return resolve(path, InterpreterSource::EnvironmentOverride, requires);
    }

    // `Bundled` names no path here: a bundled runtime is laid down by the
    // installer and reached through the search path like any other, so it falls
    // to the last rule rather than inventing a location (spec 14.11).
    if let RuntimeProfile::External(path) = profile {
        return resolve(path, InterpreterSource::RuntimeProfile, requires);
    }

    match environment.find_on_search_path() {
        Some(path) => resolve(&path, InterpreterSource::SearchPath, requires),
        None => Err(HostError::Interpreter(format!(
            "no {} on the search path, and neither {ENV_PYTHON_OVERRIDE} nor an external \
             runtime profile named one",
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
///
/// The version is never guessed from the file name: `python3.7` may be a
/// symlink to 3.12 and `python3` may be 3.6, so only the interpreter's own
/// answer decides whether plugin code may run on it.
fn probe(path: &Path, source: InterpreterSource) -> Result<Interpreter, HostError> {
    // The probe is a short-lived child that prints one line. It is not given a
    // deadline because discovery has no clock in its signature; an interpreter
    // that hangs on `sys.version_info` is beyond what the host can classify.
    let output = Command::new(path)
        .args(VERSION_PROBE_ARGS)
        .output()
        .map_err(|error| {
            HostError::Interpreter(format!(
                "the interpreter at {} could not be executed: {error}",
                path.display()
            ))
        })?;

    let Some(found) = parse_reported_version(&output.stdout) else {
        return Err(HostError::Interpreter(format!(
            "the interpreter at {} did not report a version (exit {}): {}",
            path.display(),
            match output.status.code() {
                Some(code) => code.to_string(),
                None => "signalled".to_owned(),
            },
            excerpt(&output.stderr),
        )));
    };

    Ok(Interpreter {
        path: path.to_path_buf(),
        version: found,
        source,
    })
}

/// First line of `stdout` that is a bare `major[.minor[.patch]]`.
///
/// Strict on purpose: a line that is nearly a version is rejected rather than
/// partially parsed, because a misread version silently downgrades the gate
/// that keeps plugin code off an interpreter it cannot run on.
fn parse_reported_version(stdout: &[u8]) -> Option<PythonVersion> {
    String::from_utf8_lossy(stdout)
        .lines()
        .take(PROBE_SCAN_LINES)
        .find_map(|line| parse_version(line.trim()))
}

fn parse_version(text: &str) -> Option<PythonVersion> {
    if text.is_empty() {
        return None;
    }

    let mut components = text.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next().map_or(Ok(0), str::parse).ok()?;
    let patch = components.next().map_or(Ok(0), str::parse).ok()?;
    if components.next().is_some() {
        return None;
    }

    Some(PythonVersion::new(major, minor, patch))
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
