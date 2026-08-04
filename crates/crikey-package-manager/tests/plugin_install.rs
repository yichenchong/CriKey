//! Installing, upgrading, rolling back and removing plugin packages
//! (spec 23.1, 23.3, 23.4).
//!
//! Every fixture is built through the crate's own public API and every
//! installation goes into a scratch set of [`StandardDirectories`], so the
//! tests state the layout they mean instead of depending on the machine they
//! run on. Nothing here opens a socket: the one source that would need the
//! network injects a fake [`PackageFetcher`], and the production fetcher is
//! exercised only where it refuses before a client exists.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crikey_package_manager::{
    build_package, HttpFetcher, InstallSource, InstalledPlugin, LauncherLock, PackageError, PackageFetcher,
    PluginInstaller,
};
use crikey_platform::{DirectoryConvention, DirectoryEnvironment, PluginKind, StandardDirectories};

/// Name of the two-process lock test, used to re-enter this binary as the
/// process that holds the lock.
const LOCK_HOLDER_TEST: &str =
    "a_launcher_in_another_process_makes_an_install_refuse_rather_than_replace_files";

/// Environment variable naming the state directory the holder process locks.
const LOCK_HOLDER_VARIABLE: &str = "CRIKEY_TEST_LOCK_STATE_DIR";

const BINARY_NAME: &str = "native-plugin";

// ---------------------------------------------------------------------------
// Scratch space, directories and fixtures
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-pkgmgr-install-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.join(name);
        fs::create_dir_all(&path).expect("scratch subdirectory is creatable");
        path
    }

    /// Directories rooted in this scratch space.
    ///
    /// Every root is stated with a `CRIKEY_*_DIR` override rather than left to
    /// the host's conventions, so the test asserts about paths it named.
    fn directories(&self) -> StandardDirectories {
        let environment = DirectoryEnvironment::new()
            .set("HOME", &self.path)
            .set("CRIKEY_CONFIG_DIR", self.join("config"))
            .set("CRIKEY_DATA_DIR", self.join("data"))
            .set("CRIKEY_CACHE_DIR", self.join("cache"))
            .set("CRIKEY_STATE_DIR", self.join("state"));
        StandardDirectories::resolve(DirectoryConvention::Xdg, &environment)
            .expect("scratch directories resolve")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes a native plugin source tree for the host's own platform, so the
/// compatibility check in `install_native` accepts it wherever the suite runs.
fn write_native_source(scratch: &Scratch, label: &str, id: &str, version: &str, binary: &[u8]) -> PathBuf {
    let dir = scratch.subdir(label);
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).expect("bin directory is creatable");
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"Native Fixture\"\n\
         version = \"{version}\"\n\
         runtime = \"native\"\n\
         entrypoint.{os}-{arch} = \"bin/{BINARY_NAME}\"\n\n\
         [platform]\n\
         os = [\"{os}\"]\n\
         arch = [\"{arch}\"]\n"
    );
    fs::write(dir.join("crikey.toml"), manifest).expect("manifest is writable");
    fs::write(bin.join(BINARY_NAME), binary).expect("binary is writable");
    dir
}

fn build_native_archive(scratch: &Scratch, source: &Path, label: &str) -> PathBuf {
    let archive = scratch.join(label);
    build_package(source, &archive).expect("fixture package builds");
    archive
}

fn write_modern_source(scratch: &Scratch, label: &str, id: &str, version: &str, body: &str) -> PathBuf {
    let dir = scratch.subdir(label);
    let manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"Modern Fixture\"\n\
         version = \"{version}\"\n\
         runtime = \"python\"\n\
         entrypoint = \"fixture.plugin:Plugin\"\n\n\
         [python]\n\
         requires-python = \">=3.12\"\n"
    );
    fs::write(dir.join("crikey.toml"), manifest).expect("manifest is writable");
    fs::create_dir_all(dir.join("fixture")).expect("package directory is creatable");
    fs::write(dir.join("fixture").join("plugin.py"), body).expect("module is writable");
    dir
}

/// A minimal, well-formed Keypirinha package archive.
fn write_keypirinha_package(scratch: &Scratch, name: &str, body: &str) -> PathBuf {
    let archive = scratch.join(name);
    let file = fs::File::create(&archive).expect("archive is creatable");
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    writer.start_file("plugin.py", options).expect("member starts");
    writer.write_all(body.as_bytes()).expect("member is writable");
    writer.finish().expect("archive finishes");
    archive
}

/// Every file under `root`, keyed by its path relative to `root`.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                found.insert(path.strip_prefix(root).unwrap_or(&path).to_path_buf(), bytes);
            }
        }
    }
    found
}

/// The names directly inside `root`, sorted.
fn entries(root: &Path) -> Vec<String> {
    let mut names = fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A fetcher that copies a local file and records the URL it was asked for.
#[derive(Debug)]
struct RecordingFetcher {
    payload: PathBuf,
    requested: Arc<Mutex<Vec<String>>>,
}

impl PackageFetcher for RecordingFetcher {
    fn fetch(&self, url: &str, destination: &Path) -> Result<(), PackageError> {
        self.requested
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(url.to_owned());
        fs::copy(&self.payload, destination)?;
        Ok(())
    }
}

fn install(installer: &mut PluginInstaller, source: InstallSource) -> InstalledPlugin {
    installer
        .install(&source, &mut |_plugin| Ok(()))
        .expect("the fixture installs")
}

// ---------------------------------------------------------------------------
// Source selection (spec 23.1)
// ---------------------------------------------------------------------------

#[test]
fn each_install_source_lands_in_the_root_its_runtime_is_discovered_from() {
    let scratch = Scratch::new("sources");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let native_source = write_native_source(&scratch, "native", "dev.example.native", "1.0.0", b"v1");
    let native_archive = build_native_archive(&scratch, &native_source, "native.crikeypkg");
    let native = install(&mut installer, InstallSource::Archive(native_archive));
    assert_eq!(native.kind, PluginKind::Native);
    assert_eq!(
        native.root,
        directories
            .plugin_dir(PluginKind::Native)
            .join("dev.example.native")
    );

    let modern_source = write_modern_source(&scratch, "modern", "dev.example.modern", "2.0.0", "X = 1\n");
    let modern = install(&mut installer, InstallSource::Directory(modern_source));
    assert_eq!(modern.kind, PluginKind::Modern);
    assert_eq!(modern.version, "2.0.0");
    assert_eq!(
        fs::read_to_string(modern.root.join("fixture").join("plugin.py"))
            .expect("the installed module is readable"),
        "X = 1\n"
    );

    let keypirinha = write_keypirinha_package(&scratch, "Legacy.KEYPIRINHA-PACKAGE", "PASS = 1\n");
    let legacy = install(&mut installer, InstallSource::LegacyPackage(keypirinha.clone()));
    assert_eq!(legacy.kind, PluginKind::Legacy);
    assert_eq!(legacy.id, "Legacy");
    assert_eq!(
        fs::read(&legacy.root).expect("the installed package is readable"),
        fs::read(&keypirinha).expect("the source package is readable"),
        "a Keypirinha package is installed verbatim so the legacy loader reads the publisher's bytes"
    );

    let listed = installer.list().expect("the installed set is listable");
    let described = listed
        .iter()
        .map(|plugin| (plugin.kind, plugin.id.as_str(), plugin.version.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        described,
        vec![
            (PluginKind::Legacy, "Legacy", ""),
            (PluginKind::Modern, "dev.example.modern", "2.0.0"),
            (PluginKind::Native, "dev.example.native", "1.0.0"),
        ]
    );
}

#[test]
fn a_loose_keypirinha_package_directory_installs_but_a_directory_with_no_plugin_in_it_does_not() {
    let scratch = Scratch::new("legacy-dir");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let source = scratch.subdir("Loose");
    fs::write(source.join("loose.py"), "PASS = 1\n").expect("module is writable");
    let installed = install(&mut installer, InstallSource::Directory(source));
    assert_eq!(installed.kind, PluginKind::Legacy);
    assert_eq!(installed.id, "Loose");
    assert!(installed.root.join("loose.py").is_file());

    // A directory with neither a CriKey manifest nor a Python module is not a
    // plugin, and installing it would leave the launcher scanning rubbish.
    let empty = scratch.subdir("NotAPlugin");
    fs::write(empty.join("readme.txt"), "nothing here\n").expect("file is writable");
    let refusal = installer
        .install(&InstallSource::Directory(empty), &mut |_plugin| Ok(()))
        .expect_err("a directory with no plugin in it is refused");
    assert!(
        matches!(refusal, PackageError::Manifest(_)),
        "expected a manifest refusal, got {refusal}"
    );
    assert!(!directories
        .plugin_dir(PluginKind::Legacy)
        .join("NotAPlugin")
        .exists());
}

#[test]
fn a_url_source_installs_exactly_what_the_injected_fetcher_wrote_and_touches_no_network() {
    let scratch = Scratch::new("url");
    let directories = scratch.directories();
    let source = write_native_source(&scratch, "native", "dev.example.url", "1.0.0", b"fetched");
    let archive = build_native_archive(&scratch, &source, "native.crikeypkg");
    let requested = Arc::new(Mutex::new(Vec::new()));
    let mut installer = PluginInstaller::with_fetcher(
        &directories,
        Box::new(RecordingFetcher {
            payload: archive,
            requested: Arc::clone(&requested),
        }),
    );

    let installed = install(
        &mut installer,
        InstallSource::Url("https://example.invalid/packages/plugin.crikeypkg".to_owned()),
    );
    assert_eq!(installed.id, "dev.example.url");
    assert_eq!(
        fs::read(installed.root.join("bin").join(BINARY_NAME)).expect("installed binary is readable"),
        b"fetched"
    );
    assert_eq!(
        requested
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_slice(),
        ["https://example.invalid/packages/plugin.crikeypkg".to_owned()],
        "the URL reaches the fetcher unchanged"
    );

    // The download scratch space is disposable and must not survive the call
    // that made it, or a cache sweeper inherits a growing pile of archives.
    let downloads = directories.cache_dir().join("package-downloads");
    assert!(
        entries(&downloads).is_empty(),
        "a completed install leaves no downloaded archives behind: {:?}",
        entries(&downloads)
    );
}

#[test]
fn the_default_installer_uses_the_real_http_fetcher_which_refuses_a_non_http_url_before_any_request() {
    // The only assertion about the production fetcher that can be made without
    // a socket: it is what `PluginInstaller::new` wires in, it satisfies
    // `PackageFetcher`, and it decides the scheme is unusable before a client
    // exists. A fake fetcher would not produce this refusal.
    let scratch = Scratch::new("real-fetcher");
    let directories = scratch.directories();

    let fetcher: &dyn PackageFetcher = &HttpFetcher::new();
    let destination = scratch.join("never-written");
    let direct = fetcher
        .fetch("file:///etc/passwd", &destination)
        .expect_err("a non-http URL is refused");
    assert!(
        direct.to_string().contains("not an http or https URL"),
        "unexpected refusal: {direct}"
    );
    assert!(!destination.exists(), "a refused fetch writes nothing");

    let mut installer = PluginInstaller::new(&directories);
    let through_installer = installer
        .install(
            &InstallSource::Url("file:///etc/passwd".to_owned()),
            &mut |_plugin| Ok(()),
        )
        .expect_err("the default installer refuses the same URL");
    assert!(
        through_installer.to_string().contains("not an http or https URL"),
        "the default installer is not using the real fetcher: {through_installer}"
    );
}

#[test]
fn detect_classifies_a_url_a_keypirinha_package_an_archive_and_a_directory() {
    let scratch = Scratch::new("detect");
    let directory = scratch.subdir("tree");
    let package = write_keypirinha_package(&scratch, "Thing.KEYPIRINHA-PACKAGE", "PASS = 1\n");
    let archive = scratch.join("plugin.crikeypkg");
    fs::write(&archive, b"not really an archive").expect("file is writable");

    assert_eq!(
        InstallSource::detect("https://example.invalid/p.crikeypkg").expect("a URL classifies"),
        InstallSource::Url("https://example.invalid/p.crikeypkg".to_owned())
    );
    assert_eq!(
        InstallSource::detect(directory.to_str().expect("utf-8 path")).expect("a directory classifies"),
        InstallSource::Directory(directory)
    );
    assert_eq!(
        InstallSource::detect(package.to_str().expect("utf-8 path")).expect("a package classifies"),
        InstallSource::LegacyPackage(package)
    );
    assert_eq!(
        InstallSource::detect(archive.to_str().expect("utf-8 path")).expect("an archive classifies"),
        InstallSource::Archive(archive)
    );
    let missing = InstallSource::detect(scratch.join("absent").to_str().expect("utf-8 path"))
        .expect_err("a path that does not exist is not a source");
    assert!(matches!(missing, PackageError::SourceUnavailable(_)));
}

// ---------------------------------------------------------------------------
// Native installation (spec 23.3)
// ---------------------------------------------------------------------------

#[test]
fn an_unsigned_native_binary_is_marked_on_the_installed_plugin_and_stays_marked_in_the_listing() {
    let scratch = Scratch::new("unsigned");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let source = write_native_source(&scratch, "native", "dev.example.unsigned", "1.0.0", b"payload");
    let archive = build_native_archive(&scratch, &source, "unsigned.crikeypkg");
    let installed = install(&mut installer, InstallSource::Archive(archive));
    assert!(
        installed.unsigned_binary,
        "a bin/ payload with no detached signature is unsigned"
    );
    assert!(
        installer.list().expect("listable")[0].unsigned_binary,
        "the marking survives into the listing, which is where a user sees it"
    );

    let signed_source = write_native_source(&scratch, "signed", "dev.example.signed", "1.0.0", b"payload");
    fs::write(
        signed_source.join("bin").join(format!("{BINARY_NAME}.sig")),
        b"signature\n",
    )
    .expect("signature is writable");
    let signed_archive = build_native_archive(&scratch, &signed_source, "signed.crikeypkg");
    let signed = install(&mut installer, InstallSource::Archive(signed_archive));
    assert!(!signed.unsigned_binary, "a detached signature clears the marking");
    let listed = installer.list().expect("listable");
    let signed_listed = listed
        .iter()
        .find(|plugin| plugin.id == "dev.example.signed")
        .expect("the signed plugin is listed");
    assert!(!signed_listed.unsigned_binary);
}

#[test]
fn a_native_package_built_for_another_architecture_is_refused_with_nothing_installed() {
    let scratch = Scratch::new("incompatible");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let dir = scratch.subdir("alien");
    fs::create_dir_all(dir.join("bin")).expect("bin is creatable");
    fs::write(
        dir.join("crikey.toml"),
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"dev.example.alien\"\n\
         name = \"Alien\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint.aix-s390x = \"bin/native-plugin\"\n\n\
         [platform]\n\
         os = [\"aix\"]\n\
         arch = [\"s390x\"]\n",
    )
    .expect("manifest is writable");
    fs::write(dir.join("bin").join(BINARY_NAME), b"alien").expect("binary is writable");
    let archive = build_native_archive(&scratch, &dir, "alien.crikeypkg");

    let refusal = installer
        .install(&InstallSource::Archive(archive), &mut |_plugin| Ok(()))
        .expect_err("a package for another platform is refused");
    assert!(
        matches!(refusal, PackageError::IncompatiblePlatform),
        "expected an incompatible-platform refusal, got {refusal}"
    );
    assert!(entries(&directories.plugin_dir(PluginKind::Native)).is_empty());
}

#[test]
fn installing_over_a_plugin_stops_it_by_its_own_id_before_anything_is_replaced() {
    let scratch = Scratch::new("stop");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let v1 = write_native_source(&scratch, "v1", "dev.example.stop", "1.0.0", b"first");
    let archive_v1 = build_native_archive(&scratch, &v1, "v1.crikeypkg");
    install(&mut installer, InstallSource::Archive(archive_v1));
    let root = directories
        .plugin_dir(PluginKind::Native)
        .join("dev.example.stop");

    let v2 = write_native_source(&scratch, "v2", "dev.example.stop", "2.0.0", b"second");
    let archive_v2 = build_native_archive(&scratch, &v2, "v2.crikeypkg");

    let mut stopped: Vec<(String, Vec<u8>)> = Vec::new();
    installer
        .install(&InstallSource::Archive(archive_v2), &mut |plugin| {
            // Reading the installed binary from inside the callback is what
            // makes "before" observable: the old version must still be the one
            // on disk at the moment the plugin is asked to stop.
            stopped.push((
                plugin.to_owned(),
                fs::read(root.join("bin").join(BINARY_NAME)).expect("installed binary is readable"),
            ));
            Ok(())
        })
        .expect("the upgrade installs");
    assert_eq!(stopped, vec![("dev.example.stop".to_owned(), b"first".to_vec())]);
}

#[test]
fn a_stop_that_fails_aborts_the_install_and_leaves_the_running_version_untouched() {
    let scratch = Scratch::new("stop-fails");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let v1 = write_native_source(&scratch, "v1", "dev.example.refuse", "1.0.0", b"first");
    install(
        &mut installer,
        InstallSource::Archive(build_native_archive(&scratch, &v1, "v1.crikeypkg")),
    );
    let root = directories
        .plugin_dir(PluginKind::Native)
        .join("dev.example.refuse");
    let before = snapshot(&root);

    let v2 = write_native_source(&scratch, "v2", "dev.example.refuse", "2.0.0", b"second");
    let archive_v2 = build_native_archive(&scratch, &v2, "v2.crikeypkg");
    let refusal = installer
        .install(&InstallSource::Archive(archive_v2), &mut |_plugin| {
            Err(PackageError::Install("the plugin will not stop".to_owned()))
        })
        .expect_err("an install that cannot stop the plugin does not replace it");
    assert!(matches!(refusal, PackageError::Install(_)));
    assert_eq!(snapshot(&root), before, "the running version is untouched");
    assert_eq!(
        entries(&directories.plugin_dir(PluginKind::Native)),
        vec!["dev.example.refuse".to_owned()],
        "a refused install leaves no staging directory behind"
    );
}

// ---------------------------------------------------------------------------
// Atomic update and rollback (spec 23.4)
// ---------------------------------------------------------------------------

#[test]
fn an_upgrade_retains_the_previous_version_outside_every_root_discovery_scans() {
    let scratch = Scratch::new("retain");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let v1 = write_native_source(&scratch, "v1", "dev.example.retain", "1.0.0", b"first");
    install(
        &mut installer,
        InstallSource::Archive(build_native_archive(&scratch, &v1, "v1.crikeypkg")),
    );
    let v2 = write_native_source(&scratch, "v2", "dev.example.retain", "2.0.0", b"second");
    install(
        &mut installer,
        InstallSource::Archive(build_native_archive(&scratch, &v2, "v2.crikeypkg")),
    );

    assert_eq!(
        entries(&directories.plugin_dir(PluginKind::Native)),
        vec!["dev.example.retain".to_owned()],
        "the retained version must not sit in the root discovery scans, or the \
         launcher loads the superseded copy as a second plugin"
    );
    let retained = directories
        .data_dir()
        .join("plugins")
        .join(".previous")
        .join("native")
        .join("dev.example.retain");
    assert_eq!(
        fs::read(retained.join("bin").join(BINARY_NAME)).expect("the retained binary is readable"),
        b"first"
    );
    assert_eq!(
        installer
            .list()
            .expect("listable")
            .iter()
            .map(|plugin| plugin.version.clone())
            .collect::<Vec<_>>(),
        vec!["2.0.0".to_owned()],
        "only the live version is listed"
    );
}

#[test]
fn rollback_restores_the_previous_version_byte_for_byte_and_discards_the_one_it_undid() {
    let scratch = Scratch::new("rollback");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let v1 = write_native_source(&scratch, "v1", "dev.example.rollback", "1.0.0", b"first");
    install(
        &mut installer,
        InstallSource::Archive(build_native_archive(&scratch, &v1, "v1.crikeypkg")),
    );
    let root = directories
        .plugin_dir(PluginKind::Native)
        .join("dev.example.rollback");
    let original = snapshot(&root);

    let v2 = write_native_source(&scratch, "v2", "dev.example.rollback", "2.0.0", b"second");
    install(
        &mut installer,
        InstallSource::Archive(build_native_archive(&scratch, &v2, "v2.crikeypkg")),
    );
    assert_ne!(snapshot(&root), original, "the upgrade did take effect");

    let restored = installer
        .rollback(PluginKind::Native, "dev.example.rollback")
        .expect("the retained previous version is restored");
    assert_eq!(restored.version, "1.0.0");
    assert_eq!(restored.root, root);
    assert_eq!(
        snapshot(&root),
        original,
        "the previous version is restored byte for byte"
    );
    assert_eq!(
        entries(&directories.plugin_dir(PluginKind::Native)),
        vec!["dev.example.rollback".to_owned()]
    );

    // Undoing the undo is not what "roll back" means: the version that was
    // rolled back is discarded, so a second rollback has nothing to restore.
    let again = installer
        .rollback(PluginKind::Native, "dev.example.rollback")
        .expect_err("there is nothing left to roll back to");
    assert!(matches!(again, PackageError::SourceUnavailable(_)));
}

#[test]
fn an_interrupted_update_leaves_the_previous_working_version_in_place() {
    // The update fails at the last possible moment before the swap — the
    // package is well formed right up to the point where its embedded lock no
    // longer describes its payload — so a non-atomic implementation would have
    // already written some of the new version over the old one.
    let scratch = Scratch::new("interrupted");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let v1 = write_native_source(&scratch, "v1", "dev.example.interrupted", "1.0.0", b"working");
    install(
        &mut installer,
        InstallSource::Archive(build_native_archive(&scratch, &v1, "v1.crikeypkg")),
    );
    let root = directories
        .plugin_dir(PluginKind::Native)
        .join("dev.example.interrupted");
    let before = snapshot(&root);

    let v2 = write_native_source(&scratch, "v2", "dev.example.interrupted", "2.0.0", b"broken");
    let archive_v2 = build_native_archive(&scratch, &v2, "v2.crikeypkg");
    tamper_member(&archive_v2, &format!("bin/{BINARY_NAME}"), b"substituted payload");

    let refusal = installer
        .install(&InstallSource::Archive(archive_v2), &mut |_plugin| Ok(()))
        .expect_err("a package whose payload no longer matches its lock is refused");
    assert!(
        matches!(refusal, PackageError::HashMismatch(_)),
        "expected a hash refusal, got {refusal}"
    );

    assert_eq!(
        snapshot(&root),
        before,
        "the previous working version survives intact"
    );
    assert_eq!(
        entries(&directories.plugin_dir(PluginKind::Native)),
        vec!["dev.example.interrupted".to_owned()],
        "a failed update leaves neither a staging directory nor a half-swapped root"
    );
    assert_eq!(
        installer.list().expect("listable")[0].version,
        "1.0.0",
        "the launcher still sees the version it was running"
    );
}

#[test]
fn removing_a_plugin_retains_it_so_an_accidental_removal_can_be_rolled_back() {
    let scratch = Scratch::new("remove");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let source = write_modern_source(&scratch, "modern", "dev.example.remove", "1.0.0", "X = 1\n");
    let installed = install(&mut installer, InstallSource::Directory(source));
    let contents = snapshot(&installed.root);

    let removed = installer
        .remove(PluginKind::Modern, "dev.example.remove")
        .expect("the plugin is removed");
    assert_eq!(removed.id, "dev.example.remove");
    assert!(!installed.root.exists());
    assert!(installer.list().expect("listable").is_empty());

    let restored = installer
        .rollback(PluginKind::Modern, "dev.example.remove")
        .expect("a removed plugin is the rollback target");
    assert_eq!(restored.root, installed.root);
    assert_eq!(snapshot(&installed.root), contents);

    let absent = installer
        .remove(PluginKind::Modern, "dev.example.absent")
        .expect_err("removing something that is not installed is an error, not a silent success");
    assert!(matches!(absent, PackageError::SourceUnavailable(_)));
}

#[test]
fn removing_one_runtimes_plugin_leaves_another_runtimes_plugin_of_the_same_name_alone() {
    // A plugin id is unique only within its runtime, so `legacy.notes` and
    // `modern.notes` can both be installed. Keyed by id alone, removal scans
    // the runtimes in a fixed order and deletes whichever it reaches first —
    // which is the wrong plugin whenever the user named the other one.
    let scratch = Scratch::new("remove-same-id");
    let directories = scratch.directories();
    let mut installer = PluginInstaller::new(&directories);

    let shared_id = "dev.example.notes";
    let modern = install(
        &mut installer,
        InstallSource::Directory(write_modern_source(
            &scratch,
            "modern-notes",
            shared_id,
            "1.0.0",
            "X = 1\n",
        )),
    );
    let native = install(
        &mut installer,
        InstallSource::Archive(build_native_archive(
            &scratch,
            &write_native_source(&scratch, "native-notes", shared_id, "1.0.0", b"notes"),
            "notes.crikeypkg",
        )),
    );
    assert_eq!(modern.id, native.id, "the fixture needs one id in two runtimes");
    let modern_contents = snapshot(&modern.root);

    // Remove the NATIVE copy specifically. `list()` yields Legacy, Modern,
    // Native in that order, so an id-only lookup finds the modern copy first
    // and deletes it instead. Naming the modern copy here would pass either
    // way and prove nothing.
    let removed = installer
        .remove(PluginKind::Native, shared_id)
        .expect("the native copy is removed");
    assert_eq!(removed.kind, PluginKind::Native);
    assert!(!native.root.exists(), "the runtime that was named is gone");
    assert!(
        modern.root.exists(),
        "the runtime that was not named must survive"
    );
    assert_eq!(
        snapshot(&modern.root),
        modern_contents,
        "the surviving plugin is untouched, not merely present"
    );

    let survivors = installer.list().expect("listable");
    assert_eq!(survivors.len(), 1);
    assert_eq!(survivors[0].kind, PluginKind::Modern);

    // The retained copies are keyed the same way, so a rollback must not reach
    // across runtimes either.
    let wrong_runtime = installer
        .rollback(PluginKind::Legacy, shared_id)
        .expect_err("no legacy copy was ever installed");
    assert!(matches!(wrong_runtime, PackageError::SourceUnavailable(_)));
}

// ---------------------------------------------------------------------------
// The launcher lock (spec 23.3)
// ---------------------------------------------------------------------------

#[test]
fn an_install_refuses_while_the_launcher_lock_is_held() {
    let scratch = Scratch::new("locked");
    let directories = scratch.directories();
    let source = write_modern_source(&scratch, "modern", "dev.example.locked", "1.0.0", "X = 1\n");

    let held = LauncherLock::acquire(&directories).expect("the lock is free");
    let mut installer = PluginInstaller::new(&directories);
    let refusal = installer
        .install(&InstallSource::Directory(source.clone()), &mut |_plugin| Ok(()))
        .expect_err("an install refuses while a launcher holds the lock");
    assert!(
        matches!(refusal, PackageError::LauncherRunning { .. }),
        "expected a launcher-running refusal, got {refusal}"
    );
    assert!(
        !directories
            .plugin_dir(PluginKind::Modern)
            .join("dev.example.locked")
            .exists(),
        "the refusal happens before anything is staged"
    );

    drop(held);
    install(&mut installer, InstallSource::Directory(source));
}

#[test]
fn a_launcher_in_another_process_makes_an_install_refuse_rather_than_replace_files() {
    // Two real processes, because a lock that only conflicts with itself
    // within one process proves nothing about a launcher started from a
    // desktop shortcut. The child is this same test binary re-entered through
    // the environment variable below, and the handshake is a line of output
    // rather than a sleep.
    if let Ok(state_dir) = std::env::var(LOCK_HOLDER_VARIABLE) {
        hold_lock_until_stdin_closes(Path::new(&state_dir));
        return;
    }

    let scratch = Scratch::new("two-process");
    let directories = scratch.directories();
    let state_dir = directories.state_dir().to_path_buf();
    fs::create_dir_all(&state_dir).expect("the state directory is creatable");

    let mut holder = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", LOCK_HOLDER_TEST, "--nocapture", "--test-threads=1"])
        .env(LOCK_HOLDER_VARIABLE, &state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the holder process starts");

    let mut output = BufReader::new(holder.stdout.take().expect("the holder's output is piped"));
    let mut line = String::new();
    loop {
        line.clear();
        let read = output
            .read_line(&mut line)
            .expect("the holder's output is readable");
        assert_ne!(read, 0, "the holder process exited before it took the lock");
        // `--nocapture` interleaves the handshake with the child harness's own
        // partial "test <name> ... " line, so the marker is matched inside the
        // line rather than as the whole of it.
        if line.contains("locked") {
            break;
        }
    }

    let source = write_modern_source(&scratch, "modern", "dev.example.remote", "1.0.0", "X = 1\n");
    let mut installer = PluginInstaller::new(&directories);
    let refusal = installer
        .install(&InstallSource::Directory(source), &mut |_plugin| Ok(()))
        .expect_err("an install refuses while another process holds the lock");
    match refusal {
        PackageError::LauncherRunning { pid } => assert_eq!(
            pid,
            Some(holder.id()),
            "the refusal names the process the user has to quit"
        ),
        other => panic!("expected a launcher-running refusal, got {other}"),
    }

    // Closing the holder's stdin releases it; the lock goes with the process.
    drop(holder.stdin.take());
    let status = holder.wait().expect("the holder process is waitable");
    assert!(status.success(), "the holder process failed: {status}");
    assert!(
        LauncherLock::acquire(&directories).is_ok(),
        "the lock is free once the holder exits"
    );
}

/// The holder half of the two-process lock test.
fn hold_lock_until_stdin_closes(state_dir: &Path) {
    let lock = LauncherLock::acquire_at(state_dir).expect("the holder acquires the lock");
    assert!(
        state_dir.join("launcher.lock").is_file(),
        "the lock file exists while it is held"
    );
    println!("locked");
    std::io::stdout().flush().expect("the handshake is flushable");
    let mut line = String::new();
    // Blocks until the parent drops our stdin, which is the release signal.
    let _ = std::io::stdin().read_line(&mut line);
    drop(lock);
}

// ---------------------------------------------------------------------------
// Fixture tampering
// ---------------------------------------------------------------------------

/// Rewrites one member's payload, leaving the embedded lock describing the
/// original bytes. The result is a well-formed ZIP with a valid manifest, so
/// only the per-member digests can refuse it.
fn tamper_member(archive: &Path, member: &str, bytes: &[u8]) {
    let file = fs::File::open(archive).expect("archive is readable");
    let mut zip = zip::ZipArchive::new(file).expect("archive is a ZIP");
    let mut members: Vec<(String, Vec<u8>)> = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).expect("member is readable");
        let name = entry.name().to_owned();
        let mut payload = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut payload).expect("member bytes are readable");
        members.push((name, payload));
    }
    assert!(
        members.iter().any(|(name, _)| name == member),
        "the fixture has no member {member}"
    );

    let file = fs::File::create(archive).expect("archive is rewritable");
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, payload) in members {
        writer.start_file(name.clone(), options).expect("member starts");
        let payload = if name == member { bytes.to_vec() } else { payload };
        writer.write_all(&payload).expect("member is writable");
    }
    writer.finish().expect("archive finishes");
}
