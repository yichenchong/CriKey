//! Dependency resolution against an offline index (spec 23.2).
//!
//! Resolution turns declared version specs into a hash-pinned lockfile. The
//! contracts under test are the ones a plausible bug gets wrong:
//!
//! * the PEP-440 *subset* of `==`, `>=`, `>`, `<`, `<=` (comma-joined) is
//!   honoured, and among the versions that satisfy every clause the HIGHEST is
//!   chosen — with numeric, not lexical, ordering (so `1.10.0` beats `1.2.0`);
//! * a requirement no indexed wheel satisfies — whether because the name is
//!   absent or because no version fits — is a `PackageError::Resolution`, never
//!   a silent empty pick;
//! * the produced lockfile pins name, version, AND the content hash;
//! * [`PackageIndex::from_dir`] reads the `<root>/<name>-<version>/` layout and
//!   a package's hash is the SHA-256 of its path-sorted tree digest — stable
//!   across two independent loads, and sensitive to the tree's contents.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crikey_package_manager::{resolve, Lockfile, PackageError, PackageIndex};

#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-pkgmgr-resolve-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("scratch subdirectory is creatable");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes `<root>/<name>-<version>/<name>/__init__.py`.
fn write_wheel(root: &Path, name: &str, version: &str) {
    let module = root.join(format!("{name}-{version}")).join(name);
    fs::create_dir_all(&module).expect("wheel module dir is creatable");
    fs::write(
        module.join("__init__.py"),
        format!("__version__ = \"{version}\"\n"),
    )
    .expect("wheel module is writable");
}

fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An index carrying several `acme` versions whose numeric ordering differs
/// from their lexical ordering (`1.10.0` vs `1.2.0`), plus a second package.
fn multi_version_index(scratch: &Scratch, label: &str) -> PackageIndex {
    let root = scratch.subdir(label);
    for v in ["1.0.0", "1.2.0", "1.10.0", "2.0.0"] {
        write_wheel(&root, "acme", v);
    }
    write_wheel(&root, "widget", "0.5.0");
    PackageIndex::from_dir(&root).expect("a well-formed index directory loads")
}

fn resolved(index: &PackageIndex, spec: &str) -> Lockfile {
    resolve(">=3.14", &[spec.to_owned()], index)
        .unwrap_or_else(|e| panic!("`{spec}` must resolve against the index, got {e:?}"))
}

fn acme_version(lock: &Lockfile) -> &str {
    &lock
        .packages
        .iter()
        .find(|p| p.name == "acme")
        .expect("acme is in the lockfile")
        .version
}

// ---------------------------------------------------------------------------
// Version selection
// ---------------------------------------------------------------------------

#[test]
fn a_range_picks_the_highest_matching_version_numerically() {
    let scratch = Scratch::new("highest");
    let index = multi_version_index(&scratch, "index");
    // 1.10.0 and 1.2.0 both satisfy `>=1.0,<2.0`; numeric ordering picks 1.10.0.
    assert_eq!(acme_version(&resolved(&index, "acme>=1.0,<2.0")), "1.10.0");
}

#[test]
fn each_comparison_operator_selects_the_expected_version() {
    let scratch = Scratch::new("operators");
    let index = multi_version_index(&scratch, "index");

    assert_eq!(acme_version(&resolved(&index, "acme==1.2.0")), "1.2.0");
    assert_eq!(acme_version(&resolved(&index, "acme<=1.2.0")), "1.2.0");
    assert_eq!(acme_version(&resolved(&index, "acme<2.0.0")), "1.10.0");
    assert_eq!(acme_version(&resolved(&index, "acme>1.10.0")), "2.0.0");
    assert_eq!(acme_version(&resolved(&index, "acme>=1.0")), "2.0.0");
}

#[test]
fn multiple_dependencies_are_all_pinned_in_one_lockfile() {
    let scratch = Scratch::new("multi");
    let index = multi_version_index(&scratch, "index");
    let lock = resolve(
        ">=3.14",
        &["acme>=1.0,<2.0".to_owned(), "widget==0.5.0".to_owned()],
        &index,
    )
    .expect("both dependencies are present in the index");

    assert_eq!(lock.requires_python, ">=3.14");
    assert_eq!(acme_version(&lock), "1.10.0");
    let widget = lock
        .packages
        .iter()
        .find(|p| p.name == "widget")
        .expect("widget is pinned too");
    assert_eq!(widget.version, "0.5.0");
}

#[test]
fn a_resolved_package_pins_name_version_and_content_hash() {
    let scratch = Scratch::new("pins");
    let index = multi_version_index(&scratch, "index");
    let lock = resolved(&index, "acme==1.2.0");

    let acme = &lock.packages[0];
    assert_eq!(acme.name, "acme");
    assert_eq!(acme.version, "1.2.0");
    assert!(
        is_hex_sha256(&acme.hash),
        "a pinned package carries a hex SHA-256 content hash, got {:?}",
        acme.hash
    );
}

// ---------------------------------------------------------------------------
// Unsatisfiable requirements
// ---------------------------------------------------------------------------

#[test]
fn a_requirement_no_indexed_version_satisfies_is_a_resolution_error() {
    let scratch = Scratch::new("unsat");
    let index = multi_version_index(&scratch, "index");
    let err =
        resolve(">=3.14", &["acme>=99.0".to_owned()], &index).expect_err("no indexed acme satisfies >=99.0");
    assert!(
        matches!(err, PackageError::Resolution(_)),
        "an unsatisfiable requirement is a Resolution error, got {err:?}"
    );
}

#[test]
fn a_dependency_absent_from_the_index_is_a_resolution_error() {
    let scratch = Scratch::new("absent");
    let index = multi_version_index(&scratch, "index");
    let err = resolve(">=3.14", &["ghost>=1.0".to_owned()], &index)
        .expect_err("a dependency with no indexed wheel cannot resolve");
    assert!(
        matches!(err, PackageError::Resolution(_)),
        "a missing wheel is a Resolution error, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// PackageIndex tree hashing (spec 15.3 determinism)
// ---------------------------------------------------------------------------

#[test]
fn a_package_hash_is_stable_across_two_independent_index_loads() {
    let scratch = Scratch::new("stable");
    let root = scratch.subdir("index");
    write_wheel(&root, "acme", "1.2.0");

    let first = PackageIndex::from_dir(&root).expect("first load");
    let second = PackageIndex::from_dir(&root).expect("second load");

    let hash_first = resolved(&first, "acme==1.2.0").packages[0].hash.clone();
    let hash_second = resolved(&second, "acme==1.2.0").packages[0].hash.clone();

    assert!(is_hex_sha256(&hash_first), "hash is a hex SHA-256");
    assert_eq!(
        hash_first, hash_second,
        "a tree's content hash must be deterministic across loads (path-sorted digest)"
    );
}

#[test]
fn changing_a_module_changes_the_package_hash() {
    let scratch = Scratch::new("sensitive");

    let root_a = scratch.subdir("a");
    let module = root_a.join("acme-1.2.0").join("acme");
    fs::create_dir_all(&module).unwrap();
    fs::write(module.join("__init__.py"), b"MARK = 1\n").unwrap();
    let hash_a = resolved(&PackageIndex::from_dir(&root_a).unwrap(), "acme==1.2.0").packages[0]
        .hash
        .clone();

    let root_b = scratch.subdir("b");
    let module = root_b.join("acme-1.2.0").join("acme");
    fs::create_dir_all(&module).unwrap();
    fs::write(module.join("__init__.py"), b"MARK = 2\n").unwrap();
    let hash_b = resolved(&PackageIndex::from_dir(&root_b).unwrap(), "acme==1.2.0").packages[0]
        .hash
        .clone();

    assert_ne!(
        hash_a, hash_b,
        "a content-addressed hash must change when the module tree's contents change"
    );
}
