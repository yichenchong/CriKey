//! Standard directories for configuration, data, cache and state (spec 18.3).
//!
//! Resolution is a pure function of environment variables and the target's
//! convention, so it lives here rather than in the per-OS backends: no desktop
//! API is involved, and keeping it platform-independent is what lets the
//! Windows and macOS rules be tested on any host. [`StandardDirectories::for_process`]
//! is the production entry point; [`StandardDirectories::resolve`] takes the
//! convention and an explicit environment so a test can state both.
//!
//! Every directory answers one question:
//!
//! * `config` — user-editable settings the user is expected to open (spec 21.2).
//! * `data` — installed plugins and anything whose loss uninstalls something (spec 23).
//! * `cache` — derived bytes that may be deleted at any time (spec 22).
//! * `state` — journals and history: not user-editable, but not disposable either.
//!
//! `cache` and `state` are kept apart from `data` because a cache sweeper that
//! cannot tell them apart eventually deletes an installed plugin.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crikey_core::{CoreError, Result};

/// The directory layout a platform expects.
///
/// Named for the convention rather than the operating system: Linux and the
/// BSDs share one set of rules, and a test naming [`DirectoryConvention::Xdg`]
/// is stating which rules it means rather than which kernel it runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryConvention {
    /// `%APPDATA%` and `%LOCALAPPDATA%`.
    Windows,
    /// `~/Library/Application Support` and `~/Library/Caches`.
    MacOs,
    /// The XDG base directory specification.
    Xdg,
}

impl DirectoryConvention {
    /// The convention this build targets.
    pub const fn current() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Self::Xdg
        }
    }
}

/// The environment the directories are read out of.
///
/// A snapshot rather than live `std::env` access: resolution reads several
/// variables, and a value that changed underneath a half-finished resolution
/// would produce a layout no single environment ever described. It also makes
/// every rule testable without mutating the process environment, which no
/// parallel test can do safely.
#[derive(Debug, Clone, Default)]
pub struct DirectoryEnvironment {
    variables: BTreeMap<String, OsString>,
}

impl DirectoryEnvironment {
    /// An empty environment: every lookup misses.
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of the current process environment.
    pub fn from_process() -> Self {
        Self {
            variables: env::vars_os()
                .filter_map(|(key, value)| Some((key.into_string().ok()?, value)))
                .collect(),
        }
    }

    /// Sets one variable, replacing any previous value.
    pub fn set(mut self, key: &str, value: impl AsRef<OsStr>) -> Self {
        self.variables
            .insert(key.to_owned(), value.as_ref().to_os_string());
        self
    }

    /// The value of `key`, or `None` when it is unset or empty.
    ///
    /// An empty value is treated as unset because that is what the XDG
    /// specification requires, and because `FOO=` in a shell profile is how a
    /// user unsets a variable in practice.
    ///
    /// Windows environment-variable names are case-insensitive, so under
    /// [`DirectoryConvention::Windows`] a `crikey_data_dir` an operator typed in
    /// lowercase is the same variable as `CRIKEY_DATA_DIR`; ignoring it would
    /// silently root the launcher somewhere the operator did not choose. The
    /// exact spelling is still tried first, so the ordinary case stays one map
    /// lookup and an exact match beats a differently-cased one.
    fn lookup(&self, key: &str, convention: DirectoryConvention) -> Option<&OsStr> {
        let exact = self
            .variables
            .get(key)
            .map(OsString::as_os_str)
            .filter(|value| !value.is_empty());
        if exact.is_some() || convention != DirectoryConvention::Windows {
            return exact;
        }
        self.variables
            .iter()
            .find(|(name, value)| !value.is_empty() && name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_os_str())
    }

    /// An absolute path from `key`, judged by `convention`.
    ///
    /// A relative override is refused rather than joined onto the working
    /// directory: the launcher's working directory is whatever the desktop
    /// happened to start it in, so a relative override would name a different
    /// directory depending on how the process was launched.
    ///
    /// Absoluteness is decided by the convention rather than by
    /// [`Path::is_absolute`], which answers for the host. `C:\Users\...` is an
    /// absolute Windows path whether or not the process asking runs on Windows,
    /// and resolving the Windows layout on another host is exactly what the
    /// tests do.
    fn absolute(&self, key: &str, convention: DirectoryConvention) -> Result<Option<PathBuf>> {
        let Some(value) = self.lookup(key, convention) else {
            return Ok(None);
        };
        let path = PathBuf::from(value);
        if !is_absolute_for(convention, &path) {
            return Err(CoreError::Invalid(format!(
                "`{key}` must be an absolute path, got `{}`",
                path.display()
            )));
        }
        Ok(Some(path))
    }
}

/// Whether `path` is absolute under `convention`.
///
/// Windows accepts a drive-qualified path (`C:\x`, `C:/x`) and a UNC path
/// (`\\server\share`). A bare rooted path such as `\x` is deliberately refused:
/// it is relative to the current drive, so it names a different directory
/// depending on state this process does not control.
///
/// A UNC prefix alone is not enough. `\\`, `\\server` and `\\server\` name no
/// volume, so a `CRIKEY_*_DIR` override or a platform variable spelled that way
/// would be accepted here and then handed to configuration, plugin, cache and
/// state consumers that all try to do I/O beneath it. Both the server and the
/// share component must therefore be present and non-empty.
fn is_absolute_for(convention: DirectoryConvention, path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    match convention {
        DirectoryConvention::Windows => {
            if let Some(rest) = bytes.strip_prefix(br"\\") {
                let mut components = rest.split(|byte| matches!(byte, b'\\' | b'/'));
                let server = components.next().unwrap_or_default();
                let share = components.next().unwrap_or_default();
                return !server.is_empty() && !share.is_empty();
            }
            matches!(bytes, [drive, b':', separator, ..]
                if drive.is_ascii_alphabetic() && matches!(separator, b'\\' | b'/'))
        }
        DirectoryConvention::MacOs | DirectoryConvention::Xdg => bytes.starts_with(b"/"),
    }
}

/// Where CriKey keeps configuration, data, cache and state (spec 18.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandardDirectories {
    config: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    state: PathBuf,
}

/// The directory name CriKey owns inside each platform-provided root.
const APPLICATION_DIRECTORY: &str = "crikey";

/// The same name where the platform convention is title-cased.
const APPLICATION_DIRECTORY_TITLED: &str = "CriKey";

impl StandardDirectories {
    /// The directories for this process, on this platform.
    pub fn for_process() -> Result<Self> {
        Self::resolve(
            DirectoryConvention::current(),
            &DirectoryEnvironment::from_process(),
        )
    }

    /// The directories `convention` describes, read out of `environment`.
    ///
    /// The four `CRIKEY_*_DIR` overrides win over everything: they exist so a
    /// portable install, a test, and an administrator deploying to a fixed
    /// location can each state the layout outright instead of arranging the
    /// platform's variables to produce it.
    pub fn resolve(convention: DirectoryConvention, environment: &DirectoryEnvironment) -> Result<Self> {
        let base = match convention {
            DirectoryConvention::Windows => Self::windows(environment)?,
            DirectoryConvention::MacOs => Self::macos(environment)?,
            DirectoryConvention::Xdg => Self::xdg(environment)?,
        };
        Ok(Self {
            config: environment
                .absolute("CRIKEY_CONFIG_DIR", convention)?
                .unwrap_or(base.config),
            data: environment
                .absolute("CRIKEY_DATA_DIR", convention)?
                .unwrap_or(base.data),
            cache: environment
                .absolute("CRIKEY_CACHE_DIR", convention)?
                .unwrap_or(base.cache),
            state: environment
                .absolute("CRIKEY_STATE_DIR", convention)?
                .unwrap_or(base.state),
        })
    }

    /// `%APPDATA%` for roaming settings and data, `%LOCALAPPDATA%` for the
    /// bytes that should not follow a user between machines.
    fn windows(environment: &DirectoryEnvironment) -> Result<Self> {
        let roaming = environment
            .absolute("APPDATA", DirectoryConvention::Windows)?
            .ok_or_else(|| Self::missing("APPDATA"))?
            .join(APPLICATION_DIRECTORY_TITLED);
        // A roaming profile that carried a cache would synchronise derived
        // bytes across every machine the user signs in to.
        let local = environment
            .absolute("LOCALAPPDATA", DirectoryConvention::Windows)?
            .ok_or_else(|| Self::missing("LOCALAPPDATA"))?
            .join(APPLICATION_DIRECTORY_TITLED);
        Ok(Self {
            config: roaming.clone(),
            data: roaming,
            cache: local.join("Cache"),
            state: local.join("State"),
        })
    }

    /// `~/Library`, which draws the line between "Application Support" and
    /// "Caches" but has no separate configuration or state location.
    fn macos(environment: &DirectoryEnvironment) -> Result<Self> {
        let home = Self::home(environment, DirectoryConvention::MacOs)?;
        let support = home
            .join("Library")
            .join("Application Support")
            .join(APPLICATION_DIRECTORY_TITLED);
        Ok(Self {
            config: support.clone(),
            data: support.clone(),
            cache: home
                .join("Library")
                .join("Caches")
                .join(APPLICATION_DIRECTORY_TITLED),
            // macOS offers no state location, and a journal does not belong in
            // Caches, which the system may empty at any time.
            state: support.join("State"),
        })
    }

    /// The XDG base directory specification, including its documented defaults.
    fn xdg(environment: &DirectoryEnvironment) -> Result<Self> {
        let home = Self::home(environment, DirectoryConvention::Xdg)?;
        let base = |variable: &str, fallback: &[&str]| -> Result<PathBuf> {
            let root = match environment.absolute(variable, DirectoryConvention::Xdg)? {
                Some(path) => path,
                None => fallback
                    .iter()
                    .fold(home.clone(), |path, component| path.join(component)),
            };
            Ok(root.join(APPLICATION_DIRECTORY))
        };
        Ok(Self {
            config: base("XDG_CONFIG_HOME", &[".config"])?,
            data: base("XDG_DATA_HOME", &[".local", "share"])?,
            cache: base("XDG_CACHE_HOME", &[".cache"])?,
            state: base("XDG_STATE_HOME", &[".local", "state"])?,
        })
    }

    fn home(environment: &DirectoryEnvironment, convention: DirectoryConvention) -> Result<PathBuf> {
        environment
            .absolute("HOME", convention)?
            .ok_or_else(|| Self::missing("HOME"))
    }

    fn missing(variable: &str) -> CoreError {
        CoreError::Invalid(format!(
            "the standard directories cannot be resolved: `{variable}` is not set"
        ))
    }

    /// User-editable configuration (spec 21.2).
    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    /// Installed plugins and other material whose loss removes a feature.
    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    /// Derived bytes that may be deleted at any time (spec 22).
    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// Journals and history: not user-editable, not disposable.
    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    /// Where plugins of `kind` are installed.
    pub fn plugin_dir(&self, kind: PluginKind) -> PathBuf {
        self.data.join("plugins").join(kind.directory_name())
    }
}

/// The three plugin runtimes, which are installed into separate roots.
///
/// Separate roots rather than one directory with a manifest lookup: discovery
/// runs at startup for each runtime independently, and a shared root would make
/// every provider read and reject every other provider's packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PluginKind {
    Legacy,
    Modern,
    Native,
}

impl PluginKind {
    /// The directory component this kind occupies.
    pub const fn directory_name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Modern => "modern",
            Self::Native => "native",
        }
    }

    /// Every kind, in a fixed order so callers iterate deterministically.
    pub const ALL: [Self; 3] = [Self::Legacy, Self::Modern, Self::Native];
}
