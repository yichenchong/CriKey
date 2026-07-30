//! Content-addressed environment identity and the materialising store
//! (spec 15.3, 15.4, 23.4).
//!
//! An [`EnvironmentId`] is a pure hex-SHA-256 function of the inputs that decide
//! identity, canonicalised so that dependency and build-option *order* never
//! leak into it. [`EnvironmentStore`] materialises a resolved closure into an
//! isolated site dir the first time, reuses it thereafter, refuses a package
//! whose recorded hash no longer matches the index, and is atomic: a failed
//! materialisation leaves no partial directory (spec 23.4 rollback).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::index::{hex_lower, PackageIndex};
use crate::lockfile::{LockedPackage, Lockfile};
use crate::PackageError;

/// Content-addressed environment identity (spec 15.3). Hex SHA-256 of the
/// canonical environment inputs. Two plugins with identical inputs share it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvironmentId(pub String);

/// Inputs that DECIDE environment identity (spec 15.3): python version, OS,
/// arch, locked deps (sorted), native build options (sorted).
#[derive(Debug, Clone)]
pub struct EnvironmentInputs {
    pub python_version: String,
    pub os: String,
    pub arch: String,
    pub locked: Vec<LockedPackage>,
    pub native_build_options: Vec<String>,
}

impl EnvironmentInputs {
    /// A pure, deterministic function of the inputs. Deps and native build
    /// options are canonicalised (sorted) so their listing order never changes
    /// the id, while every field — including each pinned hash — feeds it.
    pub fn environment_id(&self) -> EnvironmentId {
        let mut hasher = Sha256::new();

        hasher.update(b"python_version=");
        hasher.update(self.python_version.as_bytes());
        hasher.update(b"\nos=");
        hasher.update(self.os.as_bytes());
        hasher.update(b"\narch=");
        hasher.update(self.arch.as_bytes());
        hasher.update(b"\n");

        let mut locked = self.locked.clone();
        locked.sort_by(|a, b| (&a.name, &a.version, &a.hash).cmp(&(&b.name, &b.version, &b.hash)));
        hasher.update(b"locked=\n");
        for p in &locked {
            hasher.update(p.name.as_bytes());
            hasher.update(b"\0");
            hasher.update(p.version.as_bytes());
            hasher.update(b"\0");
            hasher.update(p.hash.as_bytes());
            hasher.update(b"\n");
        }

        let mut options = self.native_build_options.clone();
        options.sort();
        hasher.update(b"native_build_options=\n");
        for o in &options {
            hasher.update(o.as_bytes());
            hasher.update(b"\n");
        }

        EnvironmentId(hex_lower(&hasher.finalize()))
    }
}

/// A materialised, reusable managed environment.
#[derive(Debug, Clone)]
pub struct MaterializedEnvironment {
    pub id: EnvironmentId,
    pub site_dir: PathBuf,
}

/// The durable lockfile artifact written into every committed env dir (spec
/// 23.2), so a materialised environment carries a consumable record of the
/// exact closure it was built from.
const LOCKFILE_NAME: &str = "crikey-lock.toml";

/// Materialises/reuses content-addressed envs under a cache root.
#[derive(Debug, Clone)]
pub struct EnvironmentStore {
    cache_root: PathBuf,
}

impl EnvironmentStore {
    pub fn new(cache_root: PathBuf) -> EnvironmentStore {
        EnvironmentStore { cache_root }
    }

    /// The committed env directory for an id (its `site` child is the site dir).
    fn env_dir(&self, id: &EnvironmentId) -> PathBuf {
        self.cache_root.join(&id.0)
    }

    /// Report whether a committed environment for `id` exists.
    pub fn contains(&self, id: &EnvironmentId) -> bool {
        self.env_dir(id).is_dir()
    }

    /// Verify each locked hash against the index, materialise the env IF ABSENT
    /// (reusing it if present), atomically. A failure leaves no partial env dir.
    pub fn ensure(
        &self,
        inputs: &EnvironmentInputs,
        index: &PackageIndex,
    ) -> Result<MaterializedEnvironment, PackageError> {
        let id = inputs.environment_id();
        let env_dir = self.env_dir(&id);
        let site_dir = env_dir.join("site");

        // Reuse: a committed env is never re-materialised.
        if env_dir.is_dir() {
            return Ok(MaterializedEnvironment { id, site_dir });
        }

        // Verify every recorded hash against the current index before touching
        // the filesystem, so a tamper fails before any directory is created.
        for locked in &inputs.locked {
            match index.get(&locked.name, &locked.version) {
                Some(entry) if entry.hash == locked.hash => {}
                Some(entry) => {
                    return Err(PackageError::HashMismatch(format!(
                        "{}-{}: expected {}, index has {}",
                        locked.name, locked.version, locked.hash, entry.hash
                    )));
                }
                None => {
                    return Err(PackageError::HashMismatch(format!(
                        "{}-{} not present in index",
                        locked.name, locked.version
                    )));
                }
            }
        }

        std::fs::create_dir_all(&self.cache_root)?;

        // Build into a private staging dir, then rename into place. Any failure
        // wipes the staging dir, so a partial environment is never committed.
        let staging = self
            .cache_root
            .join(format!(".staging-{}-{}", std::process::id(), id.0));
        let _ = std::fs::remove_dir_all(&staging);

        let result = (|| -> Result<(), PackageError> {
            let staging_site = staging.join("site");
            std::fs::create_dir_all(&staging_site)?;
            // Copy in the SAME canonical order the environment id hashes, so an
            // env id always materialises into a byte-identical closure
            // regardless of the declared dependency order (spec 15.3).
            let mut ordered = inputs.locked.clone();
            ordered.sort_by(|a, b| (&a.name, &a.version, &a.hash).cmp(&(&b.name, &b.version, &b.hash)));
            for locked in &ordered {
                let entry = index
                    .get(&locked.name, &locked.version)
                    .expect("verified present above");
                copy_tree(&entry.root, &staging_site)?;
            }
            // Produce the durable lockfile artifact (spec 23.2) beside `site`,
            // so the committed env carries a consumable record of its closure.
            let lockfile = Lockfile {
                requires_python: inputs.python_version.clone(),
                packages: ordered,
            };
            std::fs::write(staging.join(LOCKFILE_NAME), lockfile.to_toml())?;
            Ok(())
        })();

        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        // Commit atomically. If a racing ensure beat us to it, drop our staging
        // and reuse the winner.
        if let Err(e) = std::fs::rename(&staging, &env_dir) {
            let _ = std::fs::remove_dir_all(&staging);
            if env_dir.is_dir() {
                return Ok(MaterializedEnvironment { id, site_dir });
            }
            return Err(PackageError::Io(e));
        }

        Ok(MaterializedEnvironment { id, site_dir })
    }
}

/// Recursively copy the CONTENTS of `src` into `dst`, merging module *dirs* but
/// refusing to overwrite a file: two packages that ship the same file path are
/// a cross-package collision. Silently overwriting (last-writer-wins) would let
/// declared order decide the materialised bytes and break the content-addressing
/// invariant (spec 15.3), so a collision is a typed error instead.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), PackageError> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&from, &to)?;
        } else if to.exists() {
            return Err(PackageError::Resolution(format!(
                "cross-package file collision at {}",
                to.display()
            )));
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
