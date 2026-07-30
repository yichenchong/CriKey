//! The lockfile TOML contract (spec 23.2).
//!
//! A lockfile is the durable record resolution produces and reuse consumes, so
//! it has to survive a serialise/deserialise round trip unchanged, and it has
//! to be *byte-stable*: the same closure must serialise to the same bytes every
//! time, independent of the order the packages happen to be held in memory.
//! Byte stability is not cosmetic — a lockfile whose bytes wobble would make a
//! content-addressed environment's identity wobble with it.

use crikey_package_manager::{LockedPackage, Lockfile, PackageError};

fn pkg(name: &str, version: &str, hash: &str) -> LockedPackage {
    LockedPackage {
        name: name.to_owned(),
        version: version.to_owned(),
        hash: hash.to_owned(),
    }
}

/// A lockfile whose packages are already in sorted order, so a round trip
/// through a canonicalising serialiser returns an equal value.
fn sorted_lockfile() -> Lockfile {
    Lockfile {
        requires_python: ">=3.12".to_owned(),
        packages: vec![
            pkg("acme", "1.2.0", &"a".repeat(64)),
            pkg("brine", "0.5.0", &"b".repeat(64)),
            pkg("cedar", "3.0.1", &"c".repeat(64)),
        ],
    }
}

#[test]
fn a_lockfile_round_trips_through_toml_unchanged() {
    let original = sorted_lockfile();
    let restored =
        Lockfile::from_toml(&original.to_toml()).expect("a lockfile this crate serialised must deserialise");
    assert_eq!(
        original, restored,
        "from_toml(to_toml(x)) must equal x — the lockfile is the durable record"
    );
}

#[test]
fn serialisation_is_independent_of_in_memory_package_order() {
    let ordered = sorted_lockfile();

    let mut shuffled = ordered.clone();
    shuffled.packages.reverse();

    assert_eq!(
        ordered.to_toml(),
        shuffled.to_toml(),
        "packages are canonicalised (sorted) on write, so listing order must not leak into the bytes"
    );
}

#[test]
fn a_round_tripped_lockfile_preserves_every_pinned_field() {
    let original = sorted_lockfile();
    let restored = Lockfile::from_toml(&original.to_toml()).expect("deserialises");

    assert_eq!(restored.requires_python, ">=3.12");
    assert_eq!(restored.packages.len(), 3);
    let acme = restored
        .packages
        .iter()
        .find(|p| p.name == "acme")
        .expect("acme survives the round trip");
    assert_eq!(acme.version, "1.2.0");
    assert_eq!(acme.hash, "a".repeat(64));
}

#[test]
fn malformed_toml_is_a_package_error_not_a_panic() {
    let err = Lockfile::from_toml("this is not = = valid toml [[[")
        .expect_err("garbage input must be reported, never accepted or panicked on");
    // The variant is unimportant; that it is a typed PackageError is the point.
    let _: PackageError = err;
}
