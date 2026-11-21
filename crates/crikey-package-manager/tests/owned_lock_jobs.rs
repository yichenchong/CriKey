//! P7 resident-owned-lock safety regressions; no worker or network is started.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crikey_package_manager::{InstallSource, LauncherLock, PackageError, PluginInstaller};
use crikey_platform::{DirectoryConvention, DirectoryEnvironment, PluginKind, StandardDirectories};

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "crikey-owned-lock-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("unique fixture directory");
        Self { root }
    }
    fn directories(&self, name: &str) -> StandardDirectories {
        let base = self.root.join(name);
        let environment = DirectoryEnvironment::new()
            .set("HOME", &base)
            .set("APPDATA", &base)
            .set("LOCALAPPDATA", &base)
            .set("CRIKEY_CONFIG_DIR", base.join("config"))
            .set("CRIKEY_DATA_DIR", base.join("data"))
            .set("CRIKEY_CACHE_DIR", base.join("cache"))
            .set("CRIKEY_STATE_DIR", base.join("state"));
        StandardDirectories::resolve(DirectoryConvention::current(), &environment)
            .expect("fixture directories")
    }
    fn source(&self, name: &str, version: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir(&path).expect("source directory");
        fs::write(path.join("crikey.toml"), format!(
            "manifest-version = 1\n[plugin]\nid = \"fixture\"\nname = \"Fixture\"\nversion = \"{version}\"\nruntime = \"python\"\nentrypoint = \"plugin:Plugin\"\n[python]\nrequires-python = \">=3.12\"\n"
        )).expect("manifest");
        fs::write(path.join("plugin.py"), format!("VERSION = {version:?}\n")).expect("source module");
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn contents(root: &Path) -> String {
    fs::read_to_string(root.join("plugin.py")).expect("installed module")
}

#[test]
fn resident_install_keeps_cross_process_exclusion_and_requires_stop_acknowledgment() {
    let fixture = Fixture::new();
    let directories = fixture.directories("host");
    let lock = LauncherLock::acquire(&directories).expect("resident owns lock");
    let mut installer = PluginInstaller::new(&directories);
    let first = fixture.source("first", "1.0.0");
    let second = fixture.source("second", "2.0.0");
    assert!(matches!(
        installer.install(&InstallSource::Directory(first.clone()), &mut |_| Ok(())),
        Err(PackageError::LauncherRunning { .. })
    ));
    let mut acknowledgments = 0;
    let installed = installer
        .install_under_owned_lock(&lock, &InstallSource::Directory(first), &mut |id| {
            assert_eq!(id, "fixture");
            acknowledgments += 1;
            Ok(())
        })
        .expect("acknowledged in-process install");
    assert_eq!(acknowledgments, 1);
    let old_contents = contents(&installed.root);
    let denied =
        installer.install_under_owned_lock(&lock, &InstallSource::Directory(second.clone()), &mut |_| {
            Err(PackageError::Install("owner did not reap worker".into()))
        });
    assert!(denied.is_err());
    assert_eq!(contents(&installed.root), old_contents);
    assert!(
        LauncherLock::acquire(&directories).is_err(),
        "operation never releases resident exclusion"
    );
    let upgraded = installer
        .install_under_owned_lock(&lock, &InstallSource::Directory(second), &mut |_| Ok(()))
        .expect("upgrade");
    assert_eq!(upgraded.version, "2.0.0");
    let removed = installer
        .remove_under_owned_lock(&lock, PluginKind::Modern, "fixture")
        .expect("remove");
    assert!(!removed.root.exists());
    assert!(LauncherLock::acquire(&directories).is_err());
    drop(lock);
    let restored = installer
        .rollback(PluginKind::Modern, "fixture")
        .expect("removal retains rollback target");
    assert_eq!(restored.version, "2.0.0");
}

#[test]
fn another_state_directorys_owned_lock_does_not_authorize_install_or_removal() {
    let fixture = Fixture::new();
    let first = fixture.directories("first");
    let second = fixture.directories("second");
    let first_lock = LauncherLock::acquire(&first).expect("first lock");
    let _second_lock = LauncherLock::acquire(&second).expect("second lock");
    let mut installer = PluginInstaller::new(&second);
    let source = fixture.source("source", "1.0.0");
    let mut stopped = false;
    assert!(installer
        .install_under_owned_lock(&first_lock, &InstallSource::Directory(source), &mut |_| {
            stopped = true;
            Ok(())
        })
        .is_err());
    assert!(!stopped);
    assert!(installer
        .remove_under_owned_lock(&first_lock, PluginKind::Modern, "fixture")
        .is_err());
    assert!(installer.list().expect("inventory").is_empty());
}

#[test]
fn bounded_inventory_refuses_excess_instead_of_returning_a_partial_list() {
    let fixture = Fixture::new();
    let directories = fixture.directories("host");
    let mut installer = PluginInstaller::new(&directories);
    let source = fixture.source("source", "1.0.0");
    installer
        .install(&InstallSource::Directory(source), &mut |_| Ok(()))
        .expect("install");
    assert!(installer.list_bounded(0).is_err());
    assert_eq!(installer.list_bounded(1).expect("one entry").len(), 1);
}

#[test]
fn index_checkpoint_cancels_between_complete_index_operations() {
    use crikey_package_manager::{PackageFetcher, PluginIndexClient, TrustStore};
    use std::sync::Arc;
    use std::time::Duration;
    struct CountFetches(Arc<AtomicU64>);
    impl PackageFetcher for CountFetches {
        fn fetch(&self, _url: &str, _destination: &Path) -> Result<(), PackageError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(PackageError::SourceUnavailable("test transport refused".into()))
        }
    }
    let fixture = Fixture::new();
    let directories = fixture.directories("index");
    let calls = Arc::new(AtomicU64::new(0));
    let client = PluginIndexClient::with_parts(
        directories.cache_dir(),
        vec![
            "https://first.invalid/index".into(),
            "https://second.invalid/index".into(),
        ],
        Duration::from_secs(60),
        Box::new(CountFetches(Arc::clone(&calls))),
        Box::new(CountFetches(Arc::clone(&calls))),
        TrustStore::load(&directories).expect("empty trust store"),
    );
    let mut checkpoints = 0;
    let result = client.load_with_checkpoint(true, &mut || {
        checkpoints += 1;
        if checkpoints == 2 {
            Err("cancelled")
        } else {
            Ok(())
        }
    });
    assert_eq!(result.expect_err("cancellation is returned"), "cancelled");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "second index was never contacted"
    );
}

#[test]
fn selected_index_identity_cannot_replace_another_modern_package() {
    use crikey_plugin_model::Runtime;
    use std::io::Write as _;
    let fixture = Fixture::new();
    let directories = fixture.directories("indexed");
    let original = fixture.source("original", "1.0.0");
    let installed = PluginInstaller::new(&directories)
        .install(&InstallSource::Directory(original), &mut |_| Ok(()))
        .expect("existing B");
    let old = contents(&installed.root);
    let replacement = fixture.source("replacement", "2.0.0");
    let archive = fixture.root.join("replacement.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&archive).expect("archive"));
    for name in ["crikey.toml", "plugin.py"] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .expect("member");
        zip.write_all(&fs::read(replacement.join(name)).expect("source member"))
            .expect("write member");
    }
    zip.finish().expect("real modern archive");
    let lock = LauncherLock::acquire(&directories).expect("owned lock");
    for (selected, runtime) in [("selected-A", Runtime::Python), ("fixture", Runtime::Native)] {
        let mut installer =
            PluginInstaller::new(&directories).with_expected_identity(selected.into(), runtime);
        let mut stopped = false;
        let refusal = installer
            .install_under_owned_lock(&lock, &InstallSource::Archive(archive.clone()), &mut |_| {
                stopped = true;
                Ok(())
            })
            .expect_err("identity/runtime mismatch");
        assert!(refusal.to_string().contains("indexed identity mismatch"));
        assert!(!stopped);
        assert_eq!(
            contents(&installed.root),
            old,
            "B's installed bytes are untouched"
        );
        assert!(!directories
            .plugin_dir(PluginKind::Modern)
            .join("selected-A")
            .exists());
    }
}

#[test]
fn selected_runtime_is_checked_even_when_native_and_wasm_share_the_same_root_kind() {
    use crikey_package_manager::build_package;
    use crikey_plugin_model::Runtime;
    let fixture = Fixture::new();
    let directories = fixture.directories("runtime");
    let archive = |label: &str, version: &str| {
        let source = fixture.root.join(label);
        fs::create_dir_all(source.join("bin")).expect("source");
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        fs::write(source.join("crikey.toml"), format!(
            "manifest-version = 1\n[plugin]\nid = \"fixture\"\nname = \"Fixture\"\nversion = \"{version}\"\nruntime = \"native\"\nentrypoint.{os}-{arch} = \"bin/plugin\"\n[platform]\nos = [\"{os}\"]\narch = [\"{arch}\"]\n"
        )).expect("manifest");
        fs::write(source.join("bin/plugin"), version).expect("binary");
        let result = fixture.root.join(format!("{label}.crikey"));
        build_package(&source, &result).expect("real native archive");
        result
    };
    let first = archive("first", "1.0.0");
    let installed = PluginInstaller::new(&directories)
        .install(&InstallSource::Archive(first), &mut |_| Ok(()))
        .expect("existing native B");
    let second = archive("second", "2.0.0");
    let lock = LauncherLock::acquire(&directories).expect("owned lock");
    let mut stopped = false;
    let refusal = PluginInstaller::new(&directories)
        .with_expected_identity("fixture".into(), Runtime::Wasm)
        .install_under_owned_lock(&lock, &InstallSource::Archive(second), &mut |_| {
            stopped = true;
            Ok(())
        })
        .expect_err("same root kind is not the same runtime");
    assert!(refusal.to_string().contains("indexed identity mismatch"));
    assert!(!stopped);
    assert_eq!(
        fs::read_to_string(installed.root.join("bin/plugin")).expect("old binary"),
        "1.0.0"
    );
}
