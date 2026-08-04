//! Installing, removing and rolling back plugin packages (spec 23).
//!
//! One installer serves all three plugin runtimes because the risky part is
//! identical for all of them: bytes from somewhere else have to become the
//! contents of a directory the launcher reads, without ever being *partly*
//! there. Everything here funnels into
//! [`swap_into_place`](crate::native::swap_into_place): the new version is
//! materialised somewhere the launcher never looks, and one rename makes it
//! current while the version it replaced is retained beside it.
//!
//! What differs per runtime is only how a package is recognised and what
//! validation it owes:
//!
//! * **native** — an archive with an embedded per-member lock, a
//!   platform/architecture declaration, and executables. It goes through
//!   [`install_native`](crate::install_native), which authenticates every
//!   member before anything moves.
//! * **modern** — a `crikey.toml` with `runtime = "python"` and a source tree.
//!   No embedded lock to check, because there is no binary to authenticate.
//! * **legacy** — a Keypirinha package, which carries no CriKey manifest at
//!   all. It is installed verbatim into the legacy root and read by the Legacy
//!   Compatibility Layer's own loader, which applies its own entry, size and
//!   path-escape caps at load time. Installing does not extract it, so this
//!   crate does not carry a second, less careful copy of that reader.
//!
//! # Retained previous versions
//!
//! The version an install displaces is retained under
//! `<data>/plugins/.previous/<kind>/`, *outside* every root discovery scans.
//! Keeping it as a sibling of the live installation — which is what the native
//! staging machinery does on its own — would make discovery load the old copy
//! as a second, differently-named plugin.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crikey_platform::{PluginKind, StandardDirectories};
use crikey_plugin_model::{Manifest, Runtime};

use crate::fetch::{HttpFetcher, PackageFetcher};
use crate::launcher_lock::LauncherLock;
use crate::native::{
    self, collect_directory_members, read_archive, remove_path, swap_into_place, temporary_path,
    unsigned_binary_in, write_members,
};
use crate::{build_package, inspect_package, install_native_with_retention, InstallSource, PackageError};

/// Extension of a Keypirinha package, without the dot.
///
/// Spelled here rather than borrowed from `crikey-legacy-compat`, which
/// depends on this crate through the Python host and therefore cannot be
/// depended on from it.
const LEGACY_EXTENSION: &str = "keypirinha-package";

/// The CriKey plugin manifest, in a package or a source directory.
const MANIFEST_FILE: &str = "crikey.toml";

/// Directory holding one retained previous version per plugin.
const PREVIOUS_DIRECTORY: &str = ".previous";

/// Ceiling on a package file this crate reads whole into memory.
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;

/// A plugin as it exists on disk after installation (spec 23.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPlugin {
    /// The plugin's own id: the manifest id, or a Keypirinha package's name
    /// with its extension removed. Never namespaced — the caller composes
    /// `<kind>.<id>` where it needs a workspace-unique spelling.
    pub id: String,
    /// The manifest version, or the empty string for a Keypirinha package,
    /// which has no version field to report.
    pub version: String,
    /// Which root the plugin lives in, and therefore which host runs it.
    pub kind: PluginKind,
    /// The installed directory, or the installed archive for a legacy package.
    pub root: PathBuf,
    /// Whether the package ships an executable with no detached signature
    /// (spec 23.3). Reported, never refused.
    pub unsigned_binary: bool,
}

/// Installs, removes and rolls back plugin packages (spec 23.1, 23.3, 23.4).
pub struct PluginInstaller {
    directories: StandardDirectories,
    fetcher: Box<dyn PackageFetcher>,
}

// Hand-written because a `PackageFetcher` is not required to be `Debug`: the
// trait exists so callers can supply their own, and demanding `Debug` of them
// would buy nothing but a bound to satisfy.
impl std::fmt::Debug for PluginInstaller {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginInstaller")
            .field("data_dir", &self.directories.data_dir())
            .finish_non_exhaustive()
    }
}

impl PluginInstaller {
    /// An installer writing into `directories`, fetching URLs over HTTPS.
    pub fn new(directories: &StandardDirectories) -> Self {
        Self::with_fetcher(directories, Box::new(HttpFetcher::new()))
    }

    /// An installer with a caller-supplied fetcher.
    ///
    /// This is how every test avoids the network: the fetcher is the only part
    /// of installation that talks to anything outside the filesystem.
    pub fn with_fetcher(directories: &StandardDirectories, fetcher: Box<dyn PackageFetcher>) -> Self {
        Self {
            directories: directories.clone(),
            fetcher,
        }
    }

    /// Installs or upgrades the package `source` names (spec 23.1, 23.3).
    ///
    /// `stop` is called with the plugin's id after the package has been fully
    /// validated and before anything is replaced, so a caller that can stop
    /// the plugin does so at the only point where stopping it is useful. A
    /// `stop` that fails aborts the installation with nothing moved.
    ///
    /// The launcher lock is held across the whole call: no launcher can be
    /// running when files are replaced, and none can start mid-swap.
    pub fn install(
        &mut self,
        source: &InstallSource,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        let _lock = LauncherLock::acquire(&self.directories)?;
        let scratch = Scratch::new(&self.directories)?;
        let installed = self.install_locked(source, stop, &scratch);
        scratch.discard();
        installed
    }

    /// Removes an installed plugin, retaining it as the rollback target.
    ///
    /// Removal is a rename, not a delete, for the same reason installation is:
    /// a half-deleted plugin directory is a plugin the launcher still finds
    /// and can no longer run. Retaining it also makes an accidental removal
    /// recoverable with [`Self::rollback`].
    ///
    /// Keyed by `(kind, id)` rather than by id alone. A plugin id is only
    /// unique within its runtime: nothing stops a legacy `notes` package and a
    /// modern `notes` package from being installed together, and the namespaced
    /// `legacy.notes` / `modern.notes` the user types is exactly that pair. An
    /// id-only lookup would scan the runtimes in a fixed order and delete
    /// whichever it reached first, which is the wrong plugin half the time.
    pub fn remove(&mut self, kind: PluginKind, id: &str) -> Result<InstalledPlugin, PackageError> {
        let _lock = LauncherLock::acquire(&self.directories)?;
        let installed = self
            .list()?
            .into_iter()
            .find(|plugin| plugin.kind == kind && plugin.id == id)
            .ok_or_else(|| {
                PackageError::SourceUnavailable(format!(
                    "no {} plugin `{id}` is installed",
                    kind.directory_name()
                ))
            })?;
        let previous = self.previous_path(installed.kind, &installed.root)?;
        if let Some(holder) = previous.parent() {
            fs::create_dir_all(holder)?;
        }
        remove_path(&previous);
        fs::rename(&installed.root, &previous).map_err(PackageError::Io)?;
        Ok(installed)
    }

    /// Restores the version retained by the last install or removal (spec 23.4).
    ///
    /// The version being undone is discarded rather than retained: keeping it
    /// would make the next rollback undo the undo, which is not what anyone
    /// asking to roll back means.
    ///
    /// Keyed by `(kind, id)` for the same reason as [`Self::remove`].
    pub fn rollback(&mut self, kind: PluginKind, id: &str) -> Result<InstalledPlugin, PackageError> {
        let _lock = LauncherLock::acquire(&self.directories)?;
        let previous = self.find_previous(kind, id)?;
        let file_name = previous.file_name().ok_or_else(|| {
            PackageError::Install(format!("the retained previous version of `{id}` has no name"))
        })?;
        let target = self.directories.plugin_dir(kind).join(file_name);
        swap_into_place(&target, &previous, None)?;
        self.describe(kind, &target)?
            .ok_or_else(|| PackageError::Install(format!("the restored `{id}` is not a readable plugin")))
    }

    /// Every installed plugin, ordered by kind and then by id.
    pub fn list(&self) -> Result<Vec<InstalledPlugin>, PackageError> {
        let mut installed = Vec::new();
        for kind in PluginKind::ALL {
            let root = self.directories.plugin_dir(kind);
            let entries = match fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(PackageError::Io(error)),
            };
            let mut paths = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| !is_hidden(path))
                .collect::<Vec<_>>();
            paths.sort();
            for path in paths {
                if let Some(plugin) = self.describe(kind, &path)? {
                    installed.push(plugin);
                }
            }
        }
        Ok(installed)
    }

    // -- installation -------------------------------------------------------

    fn install_locked(
        &mut self,
        source: &InstallSource,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
        scratch: &Scratch,
    ) -> Result<InstalledPlugin, PackageError> {
        match source {
            InstallSource::Directory(path) => self.install_directory(path, stop),
            InstallSource::Archive(path) => self.install_archive(path, stop),
            InstallSource::LegacyPackage(path) => self.install_legacy_archive(path, stop),
            InstallSource::Url(url) => {
                let fetched = scratch.path.join(url_file_name(url));
                self.fetcher.fetch(url, &fetched)?;
                // A fetched file is a candidate archive and nothing more: it
                // is classified and validated exactly as a local one is.
                self.install_archive(&fetched, stop)
            }
        }
    }

    fn install_directory(
        &mut self,
        path: &Path,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        let manifest_path = path.join(MANIFEST_FILE);
        if !manifest_path.is_file() {
            // No CriKey manifest: a loose Keypirinha package directory, which
            // is exactly what a Keypirinha user has on disk today.
            return self.install_legacy_directory(path, stop);
        }
        let manifest = read_manifest(&manifest_path)?;
        match manifest.plugin.runtime {
            Runtime::Native => {
                // Built into an archive first, so a directory install takes the
                // same path a published package does: the builder writes the
                // lock and installation then authenticates every member.
                let scratch = Scratch::new(&self.directories)?;
                let archive = scratch.path.join("package.crikey");
                let installed =
                    build_package(path, &archive).and_then(|_| self.install_native_archive(&archive, stop));
                scratch.discard();
                installed
            }
            Runtime::Python => {
                let members = collect_directory_members(path)?;
                self.install_modern(members, stop)
            }
            Runtime::LegacyPython => {
                let id = native::safe_id_component(&manifest.plugin.id)?;
                self.install_legacy_source(path, id, &manifest.plugin.version, stop)
            }
            other => Err(PackageError::Manifest(format!(
                "{MANIFEST_FILE} declares runtime {other:?}, which is not installable"
            ))),
        }
    }

    fn install_archive(
        &mut self,
        path: &Path,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        if has_legacy_extension(path) {
            return self.install_legacy_archive(path, stop);
        }
        let members = read_archive(path)?.members;
        let manifest = native::archive_manifest(&members)?;
        match manifest.plugin.runtime {
            Runtime::Native => self.install_native_archive(path, stop),
            Runtime::Python => self.install_modern(members, stop),
            other => Err(PackageError::Manifest(format!(
                "{MANIFEST_FILE} declares runtime {other:?}, which is not installable"
            ))),
        }
    }

    /// Native packages (spec 23.3): platform, architecture and every embedded
    /// digest are checked before the running plugin is stopped and the
    /// directory is replaced.
    fn install_native_archive(
        &mut self,
        archive: &Path,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        let report = inspect_package(archive)?;
        let id = native::safe_id_component(&report.plugin)?;
        let root = self.directories.plugin_dir(PluginKind::Native).join(id);
        let previous = self.previous_path(PluginKind::Native, &root)?;
        if let Some(holder) = previous.parent() {
            fs::create_dir_all(holder)?;
        }
        let install = install_native_with_retention(
            archive,
            &root,
            std::env::consts::OS,
            std::env::consts::ARCH,
            stop,
            Some(&previous),
        )?;
        Ok(InstalledPlugin {
            id: install.report.plugin.clone(),
            version: install.report.version.clone(),
            kind: PluginKind::Native,
            root,
            unsigned_binary: install.report.unsigned_binary,
        })
    }

    fn install_modern(
        &mut self,
        members: BTreeMap<String, native::ArchiveMember>,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        let manifest = native::archive_manifest(&members)?;
        let id = native::safe_id_component(&manifest.plugin.id)?;
        let root = self.directories.plugin_dir(PluginKind::Modern).join(id);
        let unsigned_binary = unsigned_binary_in(&members);
        self.stage_and_swap(PluginKind::Modern, &root, &manifest.plugin.id, stop, |staging| {
            write_members(&members, staging)
        })?;
        Ok(InstalledPlugin {
            id: manifest.plugin.id.clone(),
            version: manifest.plugin.version.clone(),
            kind: PluginKind::Modern,
            root,
            unsigned_binary,
        })
    }

    /// A `.keypirinha-package` archive, installed verbatim (spec 23.1).
    ///
    /// The archive is opened and its member paths are checked, so a package
    /// that could never be extracted safely is refused at install time rather
    /// than at every launcher start; it is then copied unchanged, because the
    /// Legacy Compatibility Layer's loader is the one thing entitled to
    /// interpret its contents.
    fn install_legacy_archive(
        &mut self,
        archive: &Path,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        let id = native::safe_id_component(&legacy_id(archive)?)?.to_owned();
        let size = fs::metadata(archive)?.len();
        if size > MAX_PACKAGE_BYTES {
            return Err(PackageError::MalformedArchive(format!(
                "{} is {size} bytes, over the {MAX_PACKAGE_BYTES} byte package limit",
                archive.display()
            )));
        }
        read_archive(archive)?;
        let root = self
            .directories
            .plugin_dir(PluginKind::Legacy)
            .join(format!("{id}.{LEGACY_EXTENSION}"));
        let alternate = self.directories.plugin_dir(PluginKind::Legacy).join(&id);
        self.stage_and_swap(PluginKind::Legacy, &root, &id, stop, |staging| {
            fs::copy(archive, staging)?;
            Ok(())
        })?;
        remove_path(&alternate);
        Ok(InstalledPlugin {
            id,
            version: String::new(),
            kind: PluginKind::Legacy,
            root,
            unsigned_binary: false,
        })
    }

    fn install_legacy_members(
        &mut self,
        id: &str,
        members: BTreeMap<String, native::ArchiveMember>,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        let id = native::safe_id_component(id)?.to_owned();
        let root = self.directories.plugin_dir(PluginKind::Legacy).join(&id);
        let alternate = self
            .directories
            .plugin_dir(PluginKind::Legacy)
            .join(format!("{id}.{LEGACY_EXTENSION}"));
        self.stage_and_swap(PluginKind::Legacy, &root, &id, stop, |staging| {
            write_members(&members, staging)
        })?;
        remove_path(&alternate);
        Ok(InstalledPlugin {
            id,
            version: String::new(),
            kind: PluginKind::Legacy,
            root,
            unsigned_binary: false,
        })
    }

    fn install_legacy_directory(
        &mut self,
        path: &Path,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        let id = native::safe_id_component(&legacy_id(path)?)?.to_owned();
        let members = collect_directory_members(path)?;
        if !members.keys().any(|name| name.ends_with(".py")) {
            return Err(PackageError::Manifest(format!(
                "{} has neither a {MANIFEST_FILE} nor any Python module, so it is not a plugin",
                path.display()
            )));
        }
        self.install_legacy_members(&id, members, stop)
    }

    /// Installs a migrated legacy-python directory under its manifest id.
    fn install_legacy_source(
        &mut self,
        path: &Path,
        id: &str,
        version: &str,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    ) -> Result<InstalledPlugin, PackageError> {
        let id = native::safe_id_component(id)?.to_owned();
        let members = collect_directory_members(path)?;
        if !members.keys().any(|name| name.ends_with(".py")) {
            return Err(PackageError::Manifest(format!(
                "{} has neither a {MANIFEST_FILE} nor any Python module, so it is not a plugin",
                path.display()
            )));
        }
        let root = self.directories.plugin_dir(PluginKind::Legacy).join(&id);
        let alternate = self
            .directories
            .plugin_dir(PluginKind::Legacy)
            .join(format!("{id}.{LEGACY_EXTENSION}"));
        self.stage_and_swap(PluginKind::Legacy, &root, &id, stop, |staging| {
            write_members(&members, staging)
        })?;
        remove_path(&alternate);
        Ok(InstalledPlugin {
            id: id.to_owned(),
            version: version.to_owned(),
            kind: PluginKind::Legacy,
            root,
            unsigned_binary: false,
        })
    }

    /// Materialises a new version beside the live one and swaps it in, moving
    /// whatever it displaced into the retained-previous directory.
    fn stage_and_swap(
        &self,
        kind: PluginKind,
        root: &Path,
        id: &str,
        stop: &mut dyn FnMut(&str) -> Result<(), PackageError>,
        write: impl FnOnce(&Path) -> Result<(), PackageError>,
    ) -> Result<(), PackageError> {
        let parent = native::install_parent(root)?;
        fs::create_dir_all(parent)?;
        let staging = temporary_path(parent, root, "staging");
        if let Err(error) = write(&staging) {
            remove_path(&staging);
            return Err(error);
        }
        // Only now, with a complete and valid new version staged, is it worth
        // asking the caller to stop the plugin: a failure before this point
        // costs nothing, and a stop that happened earlier would have taken the
        // plugin down for an installation that then failed anyway.
        if let Err(error) = stop(id) {
            remove_path(&staging);
            return Err(error);
        }
        let previous = self.previous_path(kind, root)?;
        if let Err(error) = swap_into_place(root, &staging, Some(&previous)) {
            remove_path(&staging);
            return Err(error);
        }
        Ok(())
    }

    // -- layout and inspection ---------------------------------------------

    /// Where the version displaced by an install of `root` is retained.
    fn previous_path(&self, kind: PluginKind, root: &Path) -> Result<PathBuf, PackageError> {
        let name = root
            .file_name()
            .ok_or_else(|| PackageError::Install(format!("{} has no name", root.display())))?;
        Ok(self.previous_root(kind).join(name))
    }

    fn previous_root(&self, kind: PluginKind) -> PathBuf {
        self.directories
            .data_dir()
            .join("plugins")
            .join(PREVIOUS_DIRECTORY)
            .join(kind.directory_name())
    }

    /// The retained previous version of `id` within `kind`.
    ///
    /// Scoped to one runtime: the retained copies are keyed the same way the
    /// installed ones are, so searching every runtime would restore a different
    /// plugin that happens to share the id.
    fn find_previous(&self, kind: PluginKind, id: &str) -> Result<PathBuf, PackageError> {
        if let Ok(entries) = fs::read_dir(self.previous_root(kind)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if artifact_id(&path).as_deref() == Some(id) {
                    return Ok(path);
                }
            }
        }
        Err(PackageError::SourceUnavailable(format!(
            "no previous version of the {} plugin `{id}` is retained",
            kind.directory_name()
        )))
    }

    /// Describes an installed artifact, or `None` when the path is not one.
    fn describe(&self, kind: PluginKind, path: &Path) -> Result<Option<InstalledPlugin>, PackageError> {
        let Some(id) = artifact_id(path) else {
            return Ok(None);
        };
        match kind {
            PluginKind::Legacy => {
                let is_package = path.is_dir() || (path.is_file() && has_legacy_extension(path));
                Ok(is_package.then(|| InstalledPlugin {
                    id,
                    version: String::new(),
                    kind,
                    root: path.to_path_buf(),
                    unsigned_binary: false,
                }))
            }
            PluginKind::Modern | PluginKind::Native => {
                let manifest_path = path.join(MANIFEST_FILE);
                if !manifest_path.is_file() {
                    return Ok(None);
                }
                let manifest = read_manifest(&manifest_path)?;
                Ok(Some(InstalledPlugin {
                    id: manifest.plugin.id.clone(),
                    version: manifest.plugin.version.clone(),
                    kind,
                    root: path.to_path_buf(),
                    unsigned_binary: directory_has_unsigned_binary(&path.join("bin")),
                }))
            }
        }
    }
}

/// A directory for bytes that must not survive the call that made them.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(directories: &StandardDirectories) -> Result<Self, PackageError> {
        // Under the cache root: a download interrupted by a power cut is
        // disposable derived data, and leaving it in the data root would let a
        // sweeper mistake it for something worth keeping (spec 22).
        let holder = directories.cache_dir().join("package-downloads");
        fs::create_dir_all(&holder)?;
        let path = temporary_path(&holder, Path::new("download"), "fetch");
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn discard(self) {
        remove_path(&self.path);
    }
}

fn read_manifest(path: &Path) -> Result<Manifest, PackageError> {
    let text = fs::read_to_string(path)?;
    Manifest::parse(&text).map_err(|error| PackageError::Manifest(error.to_string()))
}

fn has_legacy_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(LEGACY_EXTENSION))
}

/// The id an installed artifact carries in its own name.
fn artifact_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') {
        return None;
    }
    let suffix = format!(".{LEGACY_EXTENSION}");
    let stem = name.len().checked_sub(suffix.len()).and_then(|start| {
        let stem = name.get(..start)?;
        let ending = name.get(start..)?;
        ending.eq_ignore_ascii_case(&suffix).then_some(stem)
    });
    Some(match stem.filter(|stem| !stem.is_empty()) {
        Some(stem) => stem.to_owned(),
        None => name.to_owned(),
    })
}

fn legacy_id(path: &Path) -> Result<String, PackageError> {
    artifact_id(path)
        .ok_or_else(|| PackageError::SourceUnavailable(format!("{} does not name a package", path.display())))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

/// The installed-directory counterpart of `unsigned_binary_in`: an executable
/// under `bin/` with no sibling `<name>.sig`.
fn directory_has_unsigned_binary(bin: &Path) -> bool {
    let Ok(entries) = fs::read_dir(bin) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if directory_has_unsigned_binary(&path) {
                return true;
            }
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".sig") {
            continue;
        }
        if !path.with_file_name(format!("{name}.sig")).is_file() {
            return true;
        }
    }
    false
}

/// The scratch file name a URL's last path segment suggests.
///
/// Only used to preserve an extension the classifier reads; anything with a
/// separator or a parent reference in it is replaced outright, because the
/// name is joined onto a local directory.
fn url_file_name(url: &str) -> String {
    let tail = url
        .rsplit('/')
        .next()
        .map(|tail| tail.split(['?', '#']).next().unwrap_or(tail))
        .unwrap_or_default();
    if tail.is_empty() || tail == "." || tail == ".." || tail.contains(['\\', ':']) {
        return "package".to_owned();
    }
    tail.to_owned()
}
