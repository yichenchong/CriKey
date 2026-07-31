//! Red-first tests for native package archives and atomic installation
//! (spec 23.3, 23.4; acceptance 31.29, 31.30).
//!
//! The fixtures are deliberately built through the package API itself. That
//! keeps these tests focused on observable archive and installation contracts:
//! deterministic bytes, embedded member integrity, platform selection,
//! unsigned-binary marking, stop-before-replace ordering, and rollback.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crikey_package_manager::{
    build_package, inspect_package, install_native, rollback_native, verify_package, NativePackageReport,
    PackageError,
};
use sha2::{Digest, Sha256};

const BINARY_NAME: &str = "native-plugin";
const LOCK_MEMBER: &str = "crikey-package.lock";

// ---------------------------------------------------------------------------
// Scratch space and fixture archives
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-pkgmgr-native-{label}-{}-{}",
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct Fixture {
    dir: PathBuf,
    id: String,
    version: String,
    os: Vec<String>,
    arch: Vec<String>,
    binary: Vec<u8>,
    expected_entries: BTreeMap<String, u64>,
    signed: bool,
}
#[derive(Debug)]
struct FixtureSpec<'a> {
    id: &'a str,
    version: &'a str,
    os: &'a [&'a str],
    arch: &'a [&'a str],
    binary: &'a [u8],
    signed: bool,
}

fn toml_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Writes the spec-shaped native manifest and one `bin/<name>` entry.
fn write_fixture(scratch: &Scratch, label: &str, spec: FixtureSpec<'_>) -> Fixture {
    let FixtureSpec {
        id,
        version,
        os,
        arch,
        binary,
        signed,
    } = spec;
    let dir = scratch.subdir(label);
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("fixture bin directory is creatable");

    let mut manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"Native Fixture\"\n\
         version = \"{version}\"\n\
         runtime = \"native\"\n"
    );
    let mut entrypoints = BTreeSet::new();
    for platform_os in os {
        for platform_arch in arch {
            entrypoints.insert(format!("{platform_os}-{platform_arch}"));
        }
    }
    for entrypoint in entrypoints {
        manifest.push_str(&format!("entrypoint.{entrypoint} = \"bin/{BINARY_NAME}\"\n"));
    }
    manifest.push_str(&format!(
        "\n[platform]\nos = [{}]\narch = [{}]\n",
        toml_list(os),
        toml_list(arch)
    ));
    let manifest_bytes = manifest.into_bytes();
    fs::write(dir.join("crikey.toml"), &manifest_bytes).expect("fixture manifest is writable");

    let binary = binary.to_vec();
    fs::write(bin_dir.join(BINARY_NAME), &binary).expect("fixture binary is writable");

    let mut expected_entries = BTreeMap::new();
    expected_entries.insert("crikey.toml".to_owned(), manifest_bytes.len() as u64);
    expected_entries.insert(format!("bin/{BINARY_NAME}"), binary.len() as u64);
    if signed {
        fs::write(bin_dir.join(format!("{BINARY_NAME}.sig")), b"fixture-signature\n")
            .expect("fixture signature is writable");
        expected_entries.insert(
            format!("bin/{BINARY_NAME}.sig"),
            b"fixture-signature\n".len() as u64,
        );
    }

    Fixture {
        dir,
        id: id.to_owned(),
        version: version.to_owned(),
        os: os.iter().map(|value| (*value).to_owned()).collect(),
        arch: arch.iter().map(|value| (*value).to_owned()).collect(),
        binary,
        expected_entries,
        signed,
    }
}

fn build_archive(scratch: &Scratch, fixture: &Fixture, label: &str) -> (PathBuf, NativePackageReport) {
    let archive = scratch.join(label);
    let report = build_package(&fixture.dir, &archive).expect("fixture package builds");
    (archive, report)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn archive_members(archive: &Path) -> BTreeMap<String, Vec<u8>> {
    let file = fs::File::open(archive).expect("archive is readable as ZIP");
    let mut zip = zip::ZipArchive::new(file).expect("built archive is a valid ZIP");
    let mut members = BTreeMap::new();
    for index in 0..zip.len() {
        let mut member = zip.by_index(index).expect("ZIP member is readable");
        let name = member.name().to_owned();
        let mut bytes = Vec::new();
        member
            .read_to_end(&mut bytes)
            .expect("ZIP member bytes are readable");
        assert!(
            members.insert(name.clone(), bytes).is_none(),
            "archive member paths must be unique: {name}"
        );
    }
    members
}

fn assert_lock_member(archive: &Path, fixture: &Fixture) {
    let members = archive_members(archive);
    let lock_bytes = members
        .get(LOCK_MEMBER)
        .expect("archive contains its member-integrity lock");
    let lock_text = std::str::from_utf8(lock_bytes).expect("package lock is UTF-8 TOML");
    let lock: toml::Value = toml::from_str(lock_text).expect("package lock is valid TOML");
    let table = lock.as_table().expect("package lock is a TOML table");
    assert_eq!(
        table.get("plugin").and_then(toml::Value::as_str),
        Some(fixture.id.as_str())
    );
    assert_eq!(
        table.get("version").and_then(toml::Value::as_str),
        Some(fixture.version.as_str())
    );

    let entries = table
        .get("entries")
        .and_then(toml::Value::as_table)
        .expect("package lock has an entries table");
    let non_lock_members: BTreeMap<_, _> = members
        .iter()
        .filter(|(path, _)| path.as_str() != LOCK_MEMBER)
        .collect();
    for (path, bytes) in non_lock_members {
        let digest = sha256_hex(bytes);
        assert_eq!(
            entries.get(path).and_then(toml::Value::as_str),
            Some(digest.as_str()),
            "lock digest for {path} must match the member bytes"
        );
    }
}

fn assert_report_metadata(report: &NativePackageReport, fixture: &Fixture, archive: &Path) {
    assert_eq!(report.plugin, fixture.id);
    assert_eq!(report.version, fixture.version);
    assert_eq!(report.os, fixture.os);
    assert_eq!(report.arch, fixture.arch);
    let archive_bytes = fs::read(archive).expect("archive is readable");
    assert_eq!(report.hash, sha256_hex(&archive_bytes));
    assert!(
        is_hex_sha256(&report.hash),
        "archive hash must be lowercase SHA-256"
    );
    assert_eq!(report.unsigned_binary, !fixture.signed);

    let actual_entries: BTreeMap<String, u64> = report.entries.iter().cloned().collect();
    assert_eq!(
        actual_entries.len(),
        report.entries.len(),
        "archive entry list must not contain duplicate paths"
    );
    assert_eq!(
        actual_entries.len(),
        fixture.expected_entries.len() + 1,
        "the only generated metadata entry is crikey-package.lock"
    );
    for (path, bytes) in &fixture.expected_entries {
        assert_eq!(actual_entries.get(path), Some(bytes), "entry {path} is pinned");
    }
    assert!(actual_entries.contains_key(LOCK_MEMBER));

    let members = archive_members(archive);
    let member_sizes: BTreeMap<String, u64> = members
        .iter()
        .map(|(path, bytes)| (path.clone(), bytes.len() as u64))
        .collect();
    assert_eq!(
        actual_entries, member_sizes,
        "inspect reports every archive member"
    );
    assert_lock_member(archive, fixture);
}

fn package_error<T>(result: Result<T, PackageError>) -> PackageError {
    match result {
        Ok(_) => panic!("operation unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn without_panic<T>(operation: impl FnOnce() -> Result<T, PackageError>) -> Result<T, PackageError> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => panic!("package operation panicked on malformed input"),
    }
}

// ---------------------------------------------------------------------------
// Snapshots make atomicity assertions independent of implementation paths.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotEntry {
    Directory,
    File(Vec<u8>),
}

fn snapshot(root: &Path) -> BTreeMap<String, SnapshotEntry> {
    let mut result = BTreeMap::new();
    if root.exists() {
        snapshot_dir(root, root, &mut result);
    }
    result
}

fn snapshot_dir(base: &Path, current: &Path, result: &mut BTreeMap<String, SnapshotEntry>) {
    let mut entries = fs::read_dir(current)
        .expect("snapshot directory is readable")
        .map(|entry| entry.expect("snapshot entry is readable"))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(base)
            .expect("snapshot path is under its root")
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            result.insert(relative, SnapshotEntry::Directory);
            snapshot_dir(base, &path, result);
        } else {
            result.insert(
                relative,
                SnapshotEntry::File(fs::read(&path).expect("snapshot file is readable")),
            );
        }
    }
}

fn truncate_archive(archive: &Path) {
    let mut bytes = fs::read(archive).expect("archive is readable before tampering");
    assert!(!bytes.is_empty(), "built archive must contain bytes");
    bytes.pop();
    fs::write(archive, bytes).expect("tampered archive is writable");
}

// ---------------------------------------------------------------------------
// Archive construction, inspection, verification
// ---------------------------------------------------------------------------

#[test]
fn build_and_inspect_native_package_round_trips_metadata_entries_and_hash_deterministically() {
    let scratch = Scratch::new("round-trip");
    let fixture = write_fixture(
        &scratch,
        "plugin",
        FixtureSpec {
            id: "dev.example.native",
            version: "1.2.3",
            os: &["linux", "macos"],
            arch: &["aarch64", "x86_64"],
            binary: &[0, 1, 2, 3, 0xff],
            signed: false,
        },
    );

    let (first_archive, first_report) = build_archive(&scratch, &fixture, "first.crikeypkg");
    assert_report_metadata(&first_report, &fixture, &first_archive);
    let inspected = inspect_package(&first_archive).expect("built package inspects");
    assert_report_metadata(&inspected, &fixture, &first_archive);
    assert_eq!(first_report.entries, inspected.entries);
    assert_eq!(first_report.hash, inspected.hash);

    let (second_archive, second_report) = build_archive(&scratch, &fixture, "second.crikeypkg");
    assert_report_metadata(&second_report, &fixture, &second_archive);
    assert_eq!(first_report.hash, second_report.hash);
    assert_eq!(
        fs::read(first_archive).expect("first archive is readable"),
        fs::read(second_archive).expect("second archive is readable"),
        "building one directory twice must produce byte-identical archives"
    );
}

#[test]
fn verify_package_accepts_good_hash_rejects_wrong_hash_and_reports_corruption_as_package_error() {
    let scratch = Scratch::new("verify");
    let fixture = write_fixture(
        &scratch,
        "plugin",
        FixtureSpec {
            id: "dev.example.verify",
            version: "2.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"verify-binary",
            signed: false,
        },
    );
    let (archive, report) = build_archive(&scratch, &fixture, "package.crikeypkg");

    let verified = verify_package(&archive, Some(&report.hash)).expect("good package verifies");
    assert_report_metadata(&verified, &fixture, &archive);

    let mut wrong_hash = report.hash.clone();
    wrong_hash.replace_range(0..1, if wrong_hash.starts_with('0') { "1" } else { "0" });
    let wrong = package_error(verify_package(&archive, Some(&wrong_hash)));
    assert!(matches!(wrong, PackageError::HashMismatch(_)));

    truncate_archive(&archive);
    let corrupted = without_panic(|| verify_package(&archive, Some(&report.hash)));
    let _: PackageError = package_error(corrupted);
}

// ---------------------------------------------------------------------------
// Installation selection, marking, ordering, rollback, and atomicity
// ---------------------------------------------------------------------------

#[test]
fn unsigned_binary_is_marked_but_remains_installable_and_signature_clears_marker() {
    let scratch = Scratch::new("unsigned");
    let unsigned = write_fixture(
        &scratch,
        "unsigned-plugin",
        FixtureSpec {
            id: "dev.example.unsigned",
            version: "1.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"unsigned-binary",
            signed: false,
        },
    );
    let (unsigned_archive, unsigned_report) = build_archive(&scratch, &unsigned, "unsigned.crikeypkg");
    assert!(unsigned_report.unsigned_binary);

    let install_root = scratch.subdir("install");
    let installed = install_native(
        &unsigned_archive,
        &install_root,
        "linux",
        "x86_64",
        &mut |_plugin| Ok(()),
    )
    .expect("an unsigned package is marked, not refused");
    assert!(installed.report.unsigned_binary);
    assert_eq!(
        fs::read(install_root.join("bin").join(BINARY_NAME)).expect("installed binary is readable"),
        unsigned.binary
    );

    let signed = write_fixture(
        &scratch,
        "signed-plugin",
        FixtureSpec {
            id: "dev.example.signed",
            version: "1.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"signed-binary",
            signed: true,
        },
    );
    let (signed_archive, signed_report) = build_archive(&scratch, &signed, "signed.crikeypkg");
    assert!(!signed_report.unsigned_binary);
    let inspected = inspect_package(&signed_archive).expect("signed fixture inspects");
    assert!(!inspected.unsigned_binary);
}

#[test]
fn install_native_rejects_incompatible_os_without_touching_install_root() {
    let scratch = Scratch::new("platform-os");
    let fixture = write_fixture(
        &scratch,
        "plugin",
        FixtureSpec {
            id: "dev.example.platform-os",
            version: "1.0.0",
            os: &["windows"],
            arch: &["x86_64"],
            binary: b"windows-only",
            signed: false,
        },
    );
    let (archive, _) = build_archive(&scratch, &fixture, "package.crikeypkg");
    let install_root = scratch.subdir("install");
    let before = snapshot(&install_root);

    let error = package_error(install_native(
        &archive,
        &install_root,
        "linux",
        "x86_64",
        &mut |_plugin| Ok(()),
    ));
    assert!(matches!(error, PackageError::IncompatiblePlatform));
    assert_eq!(snapshot(&install_root), before);
}

#[test]
fn install_native_rejects_incompatible_architecture_without_touching_install_root() {
    let scratch = Scratch::new("platform-arch");
    let fixture = write_fixture(
        &scratch,
        "plugin",
        FixtureSpec {
            id: "dev.example.platform-arch",
            version: "1.0.0",
            os: &["linux"],
            arch: &["aarch64"],
            binary: b"arm-only",
            signed: false,
        },
    );
    let (archive, _) = build_archive(&scratch, &fixture, "package.crikeypkg");
    let install_root = scratch.subdir("install");
    let before = snapshot(&install_root);

    let error = package_error(install_native(
        &archive,
        &install_root,
        "linux",
        "x86_64",
        &mut |_plugin| Ok(()),
    ));
    assert!(matches!(error, PackageError::IncompatiblePlatform));
    assert_eq!(snapshot(&install_root), before);
}

#[test]
fn install_native_rejects_a_tampered_archive_without_writing_an_empty_root() {
    let scratch = Scratch::new("tampered-empty");
    let fixture = write_fixture(
        &scratch,
        "plugin",
        FixtureSpec {
            id: "dev.example.tampered",
            version: "1.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"tamper-me",
            signed: false,
        },
    );
    let (archive, _) = build_archive(&scratch, &fixture, "package.crikeypkg");
    truncate_archive(&archive);
    let install_root = scratch.subdir("install");

    let result =
        without_panic(|| install_native(&archive, &install_root, "linux", "x86_64", &mut |_plugin| Ok(())));
    let _: PackageError = package_error(result);
    assert!(snapshot(&install_root).is_empty());
}

#[test]
fn install_native_calls_stop_running_before_replacing_existing_files() {
    let scratch = Scratch::new("stop-order");
    let v1 = write_fixture(
        &scratch,
        "v1",
        FixtureSpec {
            id: "dev.example.stop-order",
            version: "1.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"version-one",
            signed: false,
        },
    );
    let v2 = write_fixture(
        &scratch,
        "v2",
        FixtureSpec {
            id: "dev.example.stop-order",
            version: "2.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"version-two",
            signed: false,
        },
    );
    let (archive_v1, _) = build_archive(&scratch, &v1, "v1.crikeypkg");
    let (archive_v2, _) = build_archive(&scratch, &v2, "v2.crikeypkg");
    let install_root = scratch.subdir("install");

    install_native(&archive_v1, &install_root, "linux", "x86_64", &mut |_plugin| {
        Ok(())
    })
    .expect("v1 installs");
    let before_upgrade = snapshot(&install_root);

    let mut calls = Vec::new();
    let mut observed_at_stop = None;
    install_native(&archive_v2, &install_root, "linux", "x86_64", &mut |plugin| {
        calls.push(plugin.to_owned());
        observed_at_stop = Some(snapshot(&install_root));
        Ok(())
    })
    .expect("v2 installs after the running plugin stops");

    assert_eq!(calls, vec![v2.id]);
    assert_eq!(observed_at_stop, Some(before_upgrade));
    assert_eq!(
        fs::read(install_root.join("bin").join(BINARY_NAME)).expect("upgraded binary is readable"),
        v2.binary
    );
}

#[test]
fn upgrading_native_package_retains_previous_version_and_rollback_restores_it_byte_for_byte() {
    let scratch = Scratch::new("rollback");
    let v1 = write_fixture(
        &scratch,
        "v1",
        FixtureSpec {
            id: "dev.example.rollback",
            version: "1.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"rollback-version-one",
            signed: false,
        },
    );
    let v2 = write_fixture(
        &scratch,
        "v2",
        FixtureSpec {
            id: "dev.example.rollback",
            version: "2.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"rollback-version-two",
            signed: false,
        },
    );
    let (archive_v1, _) = build_archive(&scratch, &v1, "v1.crikeypkg");
    let (archive_v2, _) = build_archive(&scratch, &v2, "v2.crikeypkg");
    let install_root = scratch.subdir("install");

    let first = install_native(&archive_v1, &install_root, "linux", "x86_64", &mut |_plugin| {
        Ok(())
    })
    .expect("v1 installs");
    assert!(first.previous.is_none());
    let v1_snapshot = snapshot(&install_root);

    let upgraded = install_native(&archive_v2, &install_root, "linux", "x86_64", &mut |_plugin| {
        Ok(())
    })
    .expect("v2 upgrades v1");
    let previous = upgraded
        .previous
        .clone()
        .expect("an upgrade retains a previous version");
    assert_eq!(snapshot(&previous), v1_snapshot);
    assert_eq!(
        fs::read(previous.join("bin").join(BINARY_NAME)).expect("retained v1 binary is readable"),
        v1.binary
    );

    rollback_native(&upgraded).expect("rollback restores retained v1");
    assert_eq!(snapshot(&install_root), v1_snapshot);
    assert_eq!(
        fs::read(install_root.join("bin").join(BINARY_NAME)).expect("rolled-back binary is readable"),
        v1.binary
    );
}

#[test]
fn stop_running_failure_is_atomic_and_leaves_no_staging_directory() {
    let scratch = Scratch::new("stop-failure");
    let v1 = write_fixture(
        &scratch,
        "v1",
        FixtureSpec {
            id: "dev.example.stop-failure",
            version: "1.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"stable-version",
            signed: false,
        },
    );
    let v2 = write_fixture(
        &scratch,
        "v2",
        FixtureSpec {
            id: "dev.example.stop-failure",
            version: "2.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"new-version",
            signed: false,
        },
    );
    let (archive_v1, _) = build_archive(&scratch, &v1, "v1.crikeypkg");
    let (archive_v2, _) = build_archive(&scratch, &v2, "v2.crikeypkg");
    let install_root = scratch.subdir("install");
    install_native(&archive_v1, &install_root, "linux", "x86_64", &mut |_plugin| {
        Ok(())
    })
    .expect("v1 installs");
    let before = snapshot(&install_root);

    let mut calls = 0;
    let result = without_panic(|| {
        install_native(&archive_v2, &install_root, "linux", "x86_64", &mut |_plugin| {
            calls += 1;
            Err(PackageError::Resolution("stop refusal".to_owned()))
        })
    });
    let _: PackageError = package_error(result);
    assert_eq!(calls, 1);
    assert_eq!(snapshot(&install_root), before);
}

#[test]
fn tampered_upgrade_is_atomic_and_leaves_previous_version_and_no_staging_directory() {
    let scratch = Scratch::new("tampered-upgrade");
    let v1 = write_fixture(
        &scratch,
        "v1",
        FixtureSpec {
            id: "dev.example.tampered-upgrade",
            version: "1.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"previous-version",
            signed: false,
        },
    );
    let v2 = write_fixture(
        &scratch,
        "v2",
        FixtureSpec {
            id: "dev.example.tampered-upgrade",
            version: "2.0.0",
            os: &["linux"],
            arch: &["x86_64"],
            binary: b"tampered-version",
            signed: false,
        },
    );
    let (archive_v1, _) = build_archive(&scratch, &v1, "v1.crikeypkg");
    let (archive_v2, _) = build_archive(&scratch, &v2, "v2.crikeypkg");
    truncate_archive(&archive_v2);
    let install_root = scratch.subdir("install");
    install_native(&archive_v1, &install_root, "linux", "x86_64", &mut |_plugin| {
        Ok(())
    })
    .expect("v1 installs");
    let before = snapshot(&install_root);

    let result = without_panic(|| {
        install_native(&archive_v2, &install_root, "linux", "x86_64", &mut |_plugin| {
            Ok(())
        })
    });
    let _: PackageError = package_error(result);
    assert_eq!(snapshot(&install_root), before);
}
