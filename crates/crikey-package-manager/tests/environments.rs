//! Content-addressed environment identity and the materialising store
//! (spec 15.3, 15.4, 23.2, 23.4; acceptance 31.20 storage half).
//!
//! These tests are written before the implementation. They pin two contracts
//! that together make managed environments safe to share:
//!
//! * [`EnvironmentInputs::environment_id`] is a *pure* function of the inputs
//!   that decide identity — two callers with identical inputs (deps supplied in
//!   any order) MUST derive the same id, and any change to a decision input
//!   MUST derive a different one. This is what lets two plugins with the same
//!   dependency closure share one environment (and one worker), and what keeps
//!   two plugins with conflicting versions apart.
//! * [`EnvironmentStore::ensure`] materialises an environment the first time,
//!   reuses it thereafter, refuses a package whose recorded hash no longer
//!   matches the index, and — the part that earns "atomic" — leaves NO partial
//!   directory behind when it fails (spec 23.4 rollback).
//!
//! Nothing here spawns Python: an environment is a directory of module trees,
//! and every contract below is about that directory's identity and contents.
//! The real interpreter importing the materialised modules is the worker
//! suite's job (acceptance 31.19/31.20 execution half).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crikey_package_manager::{
    resolve, EnvironmentInputs, EnvironmentStore, LockedPackage, Lockfile, PackageError, PackageIndex,
};

// ---------------------------------------------------------------------------
// Scratch space — a private directory removed when the test that made it ends.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-pkgmgr-env-{label}-{}-{}",
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

// ---------------------------------------------------------------------------
// Offline index fixtures
//
// A "wheel" is a directory `<root>/<name>-<version>/` holding an importable
// module tree. Each fixture module records the version it belongs to so a
// materialised environment can be checked for the *right* version, not merely
// for some directory of the right name.
// ---------------------------------------------------------------------------

/// Writes `<root>/<name>-<version>/<name>/__init__.py` carrying `__version__`.
fn write_wheel(root: &Path, name: &str, version: &str) {
    let module = root.join(format!("{name}-{version}")).join(name);
    fs::create_dir_all(&module).expect("wheel module dir is creatable");
    fs::write(
        module.join("__init__.py"),
        format!("__version__ = \"{version}\"\n"),
    )
    .expect("wheel module is writable");
}

/// A hex SHA-256 is exactly 64 lowercase hex characters. Every hash the
/// package manager produces is content-addressed, so this format is itself
/// part of the contract.
fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The inputs whose identity a materialised environment is keyed by. Only the
/// locked packages vary between tests here; the platform triple is fixed so a
/// difference in id can only come from the dependency closure.
fn inputs_for(packages: Vec<LockedPackage>) -> EnvironmentInputs {
    EnvironmentInputs {
        python_version: "3.14.0".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        locked: packages,
        native_build_options: Vec::new(),
    }
}

fn pkg(name: &str, version: &str, hash: &str) -> LockedPackage {
    LockedPackage {
        name: name.to_owned(),
        version: version.to_owned(),
        hash: hash.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// environment_id — a deterministic, canonicalising pure function (spec 15.3)
// ---------------------------------------------------------------------------

#[test]
fn an_environment_id_is_a_hex_sha256_of_its_inputs() {
    let id = inputs_for(vec![pkg("acme", "1.2.0", "a".repeat(64).as_str())]).environment_id();
    assert!(
        is_hex_sha256(&id.0),
        "an environment id must be a hex SHA-256, got {:?}",
        id.0
    );
}

#[test]
fn identical_inputs_yield_the_same_id_regardless_of_dependency_order() {
    let a = pkg("acme", "1.2.0", &"1".repeat(64));
    let b = pkg("brine", "0.5.0", &"2".repeat(64));

    let forward = inputs_for(vec![a.clone(), b.clone()]).environment_id();
    let reversed = inputs_for(vec![b, a]).environment_id();

    assert_eq!(
        forward.0, reversed.0,
        "deps decide identity as a set: their listed order must not change the id"
    );
}

#[test]
fn reordering_native_build_options_does_not_change_the_id() {
    let base = pkg("acme", "1.2.0", &"1".repeat(64));

    let mut lhs = inputs_for(vec![base.clone()]);
    lhs.native_build_options = vec!["with-simd".to_owned(), "lto".to_owned()];
    let mut rhs = inputs_for(vec![base]);
    rhs.native_build_options = vec!["lto".to_owned(), "with-simd".to_owned()];

    assert_eq!(
        lhs.environment_id().0,
        rhs.environment_id().0,
        "native build options are a sorted set, not an ordered list"
    );
}

#[test]
fn every_decision_input_changes_the_id() {
    let base = inputs_for(vec![pkg("acme", "1.2.0", &"1".repeat(64))]);
    let baseline = base.environment_id().0;

    let mut variants: Vec<(&str, EnvironmentInputs)> = Vec::new();

    let mut python = inputs_for(base.locked.clone());
    python.python_version = "3.13.0".to_owned();
    variants.push(("python_version", python));

    let mut os = inputs_for(base.locked.clone());
    os.os = "windows".to_owned();
    variants.push(("os", os));

    let mut arch = inputs_for(base.locked.clone());
    arch.arch = "aarch64".to_owned();
    variants.push(("arch", arch));

    variants.push((
        "package version",
        inputs_for(vec![pkg("acme", "1.3.0", &"1".repeat(64))]),
    ));
    variants.push((
        "package hash",
        inputs_for(vec![pkg("acme", "1.2.0", &"9".repeat(64))]),
    ));

    let mut native = inputs_for(base.locked.clone());
    native.native_build_options = vec!["lto".to_owned()];
    variants.push(("native_build_options", native));

    for (what, variant) in variants {
        assert_ne!(
            baseline,
            variant.environment_id().0,
            "changing {what} must produce a different environment id"
        );
    }
}

// ---------------------------------------------------------------------------
// ensure — materialise once, reuse after, verify hashes, roll back on failure
// ---------------------------------------------------------------------------

/// Builds an index with a single `acme` version, resolves it, and returns the
/// index, the resolved lockfile, and the inputs keyed by that lockfile.
fn resolved_env(scratch: &Scratch, version: &str) -> (PackageIndex, Lockfile, EnvironmentInputs) {
    let index_root = scratch.subdir(&format!("index-{version}"));
    write_wheel(&index_root, "acme", version);
    let index = PackageIndex::from_dir(&index_root).expect("index directory loads");

    let lock = resolve(">=3.14", &[format!("acme=={version}")], &index)
        .expect("an exact requirement present in the index resolves");
    let inputs = inputs_for(lock.packages.clone());
    (index, lock, inputs)
}

#[test]
fn ensure_materialises_the_resolved_modules_into_the_site_dir() {
    let scratch = Scratch::new("materialise");
    let (index, _lock, inputs) = resolved_env(&scratch, "1.2.0");

    let store = EnvironmentStore::new(scratch.subdir("cache"));
    let env = store.ensure(&inputs, &index).expect("first ensure materialises");

    assert_eq!(
        env.id.0,
        inputs.environment_id().0,
        "the materialised env carries the id of its inputs"
    );
    assert!(
        store.contains(&env.id),
        "after ensure the store reports the env present"
    );

    let module = env.site_dir.join("acme").join("__init__.py");
    let body = fs::read_to_string(&module).expect("the resolved package's module is present in the site dir");
    assert!(
        body.contains("1.2.0"),
        "the site dir holds the *resolved* version's module, got {body:?}"
    );
}

#[test]
fn a_second_ensure_with_the_same_inputs_reuses_the_environment() {
    let scratch = Scratch::new("reuse");
    let (index, _lock, inputs) = resolved_env(&scratch, "1.2.0");

    let store = EnvironmentStore::new(scratch.subdir("cache"));
    let first = store.ensure(&inputs, &index).expect("first ensure materialises");

    // A marker only survives if the second ensure does NOT re-materialise
    // (which would wipe and rebuild the directory).
    let marker = first.site_dir.join(".crikey-reuse-witness");
    fs::write(&marker, b"witness").expect("marker is writable into the materialised env");

    let second = store
        .ensure(&inputs, &index)
        .expect("second ensure with identical inputs succeeds");

    assert_eq!(
        first.site_dir, second.site_dir,
        "identical inputs resolve to the same site dir"
    );
    assert_eq!(first.id.0, second.id.0, "identical inputs share one id");
    assert!(
        marker.is_file(),
        "reuse must not re-materialise: the in-dir marker must survive"
    );
}

#[test]
fn ensure_rejects_a_package_whose_hash_no_longer_matches_the_index() {
    let scratch = Scratch::new("tamper");
    let index_root = scratch.subdir("index");
    write_wheel(&index_root, "acme", "1.2.0");
    let index = PackageIndex::from_dir(&index_root).expect("index loads");

    // Pin the env from a clean resolve, THEN tamper the on-disk wheel so its
    // recomputed hash diverges from the lockfile's recorded one.
    let lock = resolve(">=3.14", &["acme==1.2.0".to_owned()], &index).expect("resolves");
    let inputs = inputs_for(lock.packages.clone());

    fs::write(
        index_root.join("acme-1.2.0").join("acme").join("__init__.py"),
        b"__version__ = \"tampered\"\n",
    )
    .expect("wheel module is rewritable");
    let tampered = PackageIndex::from_dir(&index_root).expect("tampered index still loads");

    let store = EnvironmentStore::new(scratch.subdir("cache"));
    let err = store
        .ensure(&inputs, &tampered)
        .expect_err("a package whose content changed must fail verification");
    assert!(
        matches!(err, PackageError::HashMismatch(_)),
        "a hash divergence is HashMismatch, not a generic error, got {err:?}"
    );
}

#[test]
fn a_failed_ensure_leaves_no_partial_environment() {
    let scratch = Scratch::new("rollback");
    let index_root = scratch.subdir("index");
    write_wheel(&index_root, "acme", "1.2.0");
    let index = PackageIndex::from_dir(&index_root).expect("index loads");

    let lock = resolve(">=3.14", &["acme==1.2.0".to_owned()], &index).expect("resolves");
    let inputs = inputs_for(lock.packages.clone());
    let id = inputs.environment_id();

    // Force materialisation to fail by tampering after the lockfile is pinned.
    fs::write(
        index_root.join("acme-1.2.0").join("acme").join("__init__.py"),
        b"__version__ = \"tampered\"\n",
    )
    .expect("wheel module is rewritable");
    let tampered = PackageIndex::from_dir(&index_root).expect("tampered index loads");

    let cache = scratch.subdir("cache");
    let store = EnvironmentStore::new(cache.clone());
    let _ = store
        .ensure(&inputs, &tampered)
        .expect_err("ensure must fail on the tampered package");

    assert!(
        !store.contains(&id),
        "a failed ensure must not report the env present (rollback, spec 23.4)"
    );
    // No committed environment directory may carry the failed id's modules.
    let leaked = fs::read_dir(&cache)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .any(|e| e.path().join("acme").join("__init__.py").exists())
        })
        .unwrap_or(false);
    assert!(
        !leaked,
        "a failed ensure must leave no partial environment on disk"
    );
}

#[test]
fn conflicting_dependency_versions_materialise_into_distinct_environments() {
    // The storage half of acceptance 31.20: two plugins pinning conflicting
    // `acme` versions get two ids and two site dirs, each holding its OWN
    // version. (The real interpreter importing the right one is the worker
    // suite's execution half.)
    let scratch = Scratch::new("coexist");
    let index_root = scratch.subdir("index");
    write_wheel(&index_root, "acme", "1.4.0");
    write_wheel(&index_root, "acme", "2.1.0");
    let index = PackageIndex::from_dir(&index_root).expect("index loads");

    let lock_v1 = resolve(">=3.14", &["acme==1.4.0".to_owned()], &index).expect("v1 resolves");
    let lock_v2 = resolve(">=3.14", &["acme==2.1.0".to_owned()], &index).expect("v2 resolves");
    let inputs_v1 = inputs_for(lock_v1.packages.clone());
    let inputs_v2 = inputs_for(lock_v2.packages.clone());

    assert_ne!(
        inputs_v1.environment_id().0,
        inputs_v2.environment_id().0,
        "conflicting versions must not collide onto one environment id"
    );

    let store = EnvironmentStore::new(scratch.subdir("cache"));
    let env_v1 = store.ensure(&inputs_v1, &index).expect("v1 materialises");
    let env_v2 = store.ensure(&inputs_v2, &index).expect("v2 materialises");

    assert_ne!(
        env_v1.site_dir, env_v2.site_dir,
        "distinct environments must occupy distinct site dirs"
    );

    let body_v1 =
        fs::read_to_string(env_v1.site_dir.join("acme").join("__init__.py")).expect("v1 module present");
    let body_v2 =
        fs::read_to_string(env_v2.site_dir.join("acme").join("__init__.py")).expect("v2 module present");
    assert!(body_v1.contains("1.4.0"), "v1 env holds acme 1.4.0");
    assert!(body_v2.contains("2.1.0"), "v2 env holds acme 2.1.0");
}

// ---------------------------------------------------------------------------
// ensure — deterministic bytes, mid-materialisation rollback, durable lockfile
// ---------------------------------------------------------------------------

/// Writes `<root>/<name>-<version>/<rel>` carrying `body`, so two packages can
/// be made to ship the SAME importable path (a cross-package collision).
fn write_package_file(root: &Path, name: &str, version: &str, rel: &str, body: &str) {
    let path = root.join(format!("{name}-{version}")).join(rel);
    fs::create_dir_all(path.parent().expect("a wheel file has a parent dir"))
        .expect("wheel dir is creatable");
    fs::write(&path, body).expect("wheel file is writable");
}

/// Builds an index with two packages that both ship the same importable file
/// path, resolves both, and returns the index plus inputs whose closure
/// contains BOTH — so materialising it copies one, then collides on the other
/// (a failure that only happens DURING copy, after every hash has verified).
fn colliding_env(scratch: &Scratch) -> (PackageIndex, EnvironmentInputs) {
    let index_root = scratch.subdir("index");
    write_package_file(&index_root, "alpha", "1.0.0", "shared/mod.py", "alpha\n");
    write_package_file(&index_root, "beta", "1.0.0", "shared/mod.py", "beta\n");
    let index = PackageIndex::from_dir(&index_root).expect("index loads");

    let lock_alpha = resolve(">=3.14", &["alpha==1.0.0".to_owned()], &index).expect("alpha resolves");
    let lock_beta = resolve(">=3.14", &["beta==1.0.0".to_owned()], &index).expect("beta resolves");
    let mut packages = lock_alpha.packages;
    packages.extend(lock_beta.packages);
    (index, inputs_for(packages))
}

#[test]
fn ensure_rejects_a_cross_package_file_collision_instead_of_overwriting() {
    // Deterministic materialised bytes (spec 15.3): two packages shipping the
    // same file path must be a typed error, not a silent last-writer-wins whose
    // outcome depends on the declared order.
    let scratch = Scratch::new("collision");
    let (index, inputs) = colliding_env(&scratch);

    let store = EnvironmentStore::new(scratch.subdir("cache"));
    let err = store
        .ensure(&inputs, &index)
        .expect_err("two packages shipping the same file path must not silently overwrite");
    assert!(
        matches!(err, PackageError::Resolution(_)),
        "a cross-package file collision is a typed PackageError, not a silent overwrite, got {err:?}"
    );
}

#[test]
fn a_failed_ensure_during_materialisation_leaves_no_env_or_staging() {
    // Unlike the hash PRE-CHECK failure, this forces a failure DURING the copy
    // (after every hash verifies) so the copy/rename rollback — and the staging
    // cleanup the docstring advertises — is actually exercised.
    let scratch = Scratch::new("rollback-mid");
    let (index, inputs) = colliding_env(&scratch);
    let id = inputs.environment_id();

    let cache = scratch.subdir("cache");
    let store = EnvironmentStore::new(cache.clone());
    let _ = store
        .ensure(&inputs, &index)
        .expect_err("a collision mid-copy must fail after staging holds partial content");

    // (a) No committed env dir for the failed id.
    assert!(
        !store.contains(&id),
        "a mid-materialisation failure must not commit an env dir (rollback, spec 23.4)"
    );
    assert!(
        !cache.join(&id.0).exists(),
        "no env dir may survive under the failed id"
    );

    // (b) No leaked staging dir under the REAL staging layout (`<staging>/site`).
    let leaked_staging: Vec<PathBuf> = fs::read_dir(&cache)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(".staging-"))
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leaked_staging.is_empty(),
        "the copy-failure rollback must remove the staging dir, leaked: {leaked_staging:?}"
    );
    let leaked_site = fs::read_dir(&cache)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .any(|e| e.path().join("site").join("shared").join("mod.py").exists())
        })
        .unwrap_or(false);
    assert!(
        !leaked_site,
        "no partial `<staging>/site/...` tree may survive a failed ensure"
    );
}

#[test]
fn ensure_writes_a_durable_lockfile_that_round_trips() {
    // §23.2: a materialised env must carry a durable, consumable lockfile — not
    // merely an in-memory one.
    let scratch = Scratch::new("lockfile-artifact");
    let (index, _lock, inputs) = resolved_env(&scratch, "1.2.0");

    let store = EnvironmentStore::new(scratch.subdir("cache"));
    let env = store.ensure(&inputs, &index).expect("ensure materialises");

    let lock_path = env
        .site_dir
        .parent()
        .expect("the site dir lives inside the committed env dir")
        .join("crikey-lock.toml");
    assert!(
        lock_path.is_file(),
        "ensure must write a durable crikey-lock.toml into the committed env dir (spec 23.2)"
    );

    let text = fs::read_to_string(&lock_path).expect("the durable lockfile is readable");
    let restored = Lockfile::from_toml(&text).expect("the written lockfile round-trips through TOML");
    let names: Vec<&str> = restored.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["acme"],
        "the durable lockfile records the resolved closure, got {restored:?}"
    );
    assert_eq!(
        restored.packages[0].version, "1.2.0",
        "the durable lockfile pins the resolved version"
    );
}
