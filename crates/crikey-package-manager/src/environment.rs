//! Content-addressed environment identity and the materialising store
//! (spec 15.3, 15.4, 23.4).
//!
//! An [`EnvironmentId`] is a pure hex-SHA-256 function of the inputs that decide
//! identity, canonicalised so that dependency and build-option *order* never
//! leak into it. [`EnvironmentStore`] materialises a resolved closure into an
//! isolated site dir the first time, reuses it thereafter, refuses a package
//! whose recorded hash no longer matches the index, and is atomic: a failed
//! materialisation leaves no partial directory (spec 23.4 rollback).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::index::{hex_lower, normalize_name, PackageIndex};
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
        locked.sort_by(|a, b| {
            (normalize_name(&a.name), &a.version, a.hash.to_ascii_lowercase()).cmp(&(
                normalize_name(&b.name),
                &b.version,
                b.hash.to_ascii_lowercase(),
            ))
        });
        hasher.update(b"locked=\n");
        for p in &locked {
            hasher.update(normalize_name(&p.name).as_bytes());
            hasher.update(b"\0");
            hasher.update(p.version.as_bytes());
            hasher.update(b"\0");
            hasher.update(p.hash.to_ascii_lowercase().as_bytes());
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
        std::fs::symlink_metadata(self.env_dir(id))
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false)
    }

    /// Verify all source and cached bytes, then materialise the env if absent.
    ///
    /// A committed cache entry is untrusted input: it is checked against the
    /// current source trees and its durable lockfile before reuse. A mismatch is
    /// refused rather than rebuilt in place, because a predictable cache path
    /// may be controlled by another process. New materialisations use an
    /// exclusive, per-call staging directory and an atomic rename.
    pub fn ensure(
        &self,
        inputs: &EnvironmentInputs,
        index: &PackageIndex,
    ) -> Result<MaterializedEnvironment, PackageError> {
        let id = inputs.environment_id();
        let env_dir = self.env_dir(&id);
        let site_dir = env_dir.join("site");
        let verified = verify_packages(inputs, index)?;

        self.ensure_cache_root()?;

        if env_dir.exists() {
            verify_committed(&env_dir, inputs, &verified)?;
            return Ok(MaterializedEnvironment { id, site_dir });
        }

        let staging = create_staging_dir(&self.cache_root, &id)?;
        let result = (|| -> Result<(), PackageError> {
            let staging_site = staging.join("site");
            std::fs::create_dir_all(&staging_site)?;

            let mut ordered = inputs.locked.clone();
            ordered.sort_by(package_order);
            for locked in &ordered {
                let entry = index.get(&locked.name, &locked.version).ok_or_else(|| {
                    PackageError::HashMismatch(format!(
                        "{}-{} not present in index",
                        locked.name, locked.version
                    ))
                })?;
                copy_tree(&entry.root, &staging_site)?;
            }

            let lockfile = Lockfile {
                requires_python: inputs.python_version.clone(),
                packages: ordered,
            };
            lockfile.validate()?;
            std::fs::write(staging.join(LOCKFILE_NAME), lockfile.to_toml())?;
            verify_committed(&staging, inputs, &verified)?;
            Ok(())
        })();

        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }

        // Commit atomically. If a racing ensure won, verify its bytes rather
        // than trusting whatever directory appeared at the cache path.
        if let Err(error) = std::fs::rename(&staging, &env_dir) {
            let _ = std::fs::remove_dir_all(&staging);
            if env_dir.exists() {
                verify_committed(&env_dir, inputs, &verified)?;
                return Ok(MaterializedEnvironment { id, site_dir });
            }
            return Err(PackageError::Io(error));
        }

        Ok(MaterializedEnvironment { id, site_dir })
    }

    fn ensure_cache_root(&self) -> Result<(), PackageError> {
        match std::fs::symlink_metadata(&self.cache_root) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(PackageError::Install(format!(
                        "cache root {} is not a real directory",
                        self.cache_root.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&self.cache_root)?;
            }
            Err(error) => return Err(PackageError::Io(error)),
        }

        #[cfg(unix)]
        {
            let metadata = std::fs::symlink_metadata(&self.cache_root)?;
            let mode = metadata.permissions().mode();
            if mode & 0o002 != 0 {
                return Err(PackageError::Install(format!(
                    "cache root {} is world-writable",
                    self.cache_root.display()
                )));
            }
            if mode & 0o077 != 0 {
                let mut permissions = metadata.permissions();
                permissions.set_mode(mode & !0o077);
                std::fs::set_permissions(&self.cache_root, permissions)?;
            }
        }
        Ok(())
    }
}

fn verify_packages<'a, 'b>(
    inputs: &'b EnvironmentInputs,
    index: &'a PackageIndex,
) -> Result<Vec<(&'a crate::index::IndexedPackage, &'b LockedPackage)>, PackageError> {
    let lockfile = Lockfile {
        requires_python: inputs.python_version.clone(),
        packages: inputs.locked.clone(),
    };
    lockfile.validate()?;

    let mut verified = Vec::with_capacity(inputs.locked.len());
    for locked in &inputs.locked {
        let entry = index.get(&locked.name, &locked.version).ok_or_else(|| {
            PackageError::HashMismatch(format!("{}-{} not present in index", locked.name, locked.version))
        })?;
        let actual = match crate::index::tree_hash(&entry.root) {
            Ok(hash) => hash,
            Err(PackageError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(PackageError::SourceUnavailable(format!(
                    "{}-{} at {}",
                    locked.name,
                    locked.version,
                    entry.root.display()
                )));
            }
            Err(error) => return Err(error),
        };
        if !actual.eq_ignore_ascii_case(&locked.hash) || !actual.eq_ignore_ascii_case(&entry.hash) {
            return Err(PackageError::HashMismatch(format!(
                "{}-{}: recorded {}, current bytes hash {}",
                locked.name, locked.version, locked.hash, actual
            )));
        }
        verified.push((entry, locked));
    }
    Ok(verified)
}

fn verify_committed(
    env_dir: &Path,
    inputs: &EnvironmentInputs,
    verified: &[(&crate::index::IndexedPackage, &LockedPackage)],
) -> Result<(), PackageError> {
    let metadata = std::fs::symlink_metadata(env_dir).map_err(PackageError::Io)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PackageError::HashMismatch(format!(
            "cached environment {} is not a real directory",
            env_dir.display()
        )));
    }

    let expected_root = BTreeSet::from([LOCKFILE_NAME, "site"]);
    for entry in std::fs::read_dir(env_dir).map_err(PackageError::Io)? {
        let entry = entry.map_err(PackageError::Io)?;
        let name = entry.file_name();
        if !expected_root.contains(name.to_string_lossy().as_ref()) {
            return Err(PackageError::HashMismatch(format!(
                "cached environment contains unexpected entry {}",
                entry.path().display()
            )));
        }
    }

    let lock_text = std::fs::read_to_string(env_dir.join(LOCKFILE_NAME)).map_err(|error| {
        PackageError::HashMismatch(format!("cached environment lockfile is unreadable: {error}"))
    })?;
    let lockfile = Lockfile::from_toml(&lock_text)?;
    if lockfile.requires_python != inputs.python_version {
        return Err(PackageError::HashMismatch(
            "cached lockfile disagrees with the requested Python version".to_owned(),
        ));
    }
    let mut expected_packages = inputs.locked.clone();
    expected_packages.sort_by(package_order);
    let mut actual_packages = lockfile.packages.clone();
    actual_packages.sort_by(package_order);
    if expected_packages.len() != actual_packages.len()
        || expected_packages
            .iter()
            .zip(&actual_packages)
            .any(|(expected, actual)| {
                crate::index::normalize_name(&expected.name) != crate::index::normalize_name(&actual.name)
                    || expected.version != actual.version
                    || !expected.hash.eq_ignore_ascii_case(&actual.hash)
            })
    {
        return Err(PackageError::HashMismatch(
            "cached lockfile disagrees with the requested dependencies".to_owned(),
        ));
    }

    let mut expected_files = BTreeMap::new();
    for (entry, _) in verified {
        collect_expected_files(&entry.root, &entry.root, &mut expected_files)?;
    }
    let site_dir = env_dir.join("site");
    let mut actual_files = BTreeMap::new();
    collect_cached_files(&site_dir, &site_dir, &mut actual_files)?;
    if expected_files != actual_files {
        let expected_names = expected_files.keys().collect::<BTreeSet<_>>();
        let actual_names = actual_files.keys().collect::<BTreeSet<_>>();
        return Err(PackageError::HashMismatch(format!(
            "cached site files differ (missing: {:?}, unexpected: {:?})",
            expected_names.difference(&actual_names).collect::<Vec<_>>(),
            actual_names.difference(&expected_names).collect::<Vec<_>>()
        )));
    }
    Ok(())
}

fn package_order(a: &LockedPackage, b: &LockedPackage) -> std::cmp::Ordering {
    (
        crate::index::normalize_name(&a.name),
        &a.version,
        a.hash.to_ascii_lowercase(),
    )
        .cmp(&(
            crate::index::normalize_name(&b.name),
            &b.version,
            b.hash.to_ascii_lowercase(),
        ))
}

fn create_staging_dir(cache_root: &Path, id: &EnvironmentId) -> Result<PathBuf, PackageError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let candidate = cache_root.join(format!(
            ".staging-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            id.0
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(PackageError::Io(error)),
        }
    }
}

fn collect_expected_files(
    base: &Path,
    dir: &Path,
    files: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), PackageError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::HashMismatch(format!(
                "indexed package contains symbolic link {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_expected_files(base, &path, files)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(base).map_err(|_| {
                PackageError::HashMismatch(format!("indexed package path escaped {}", base.display()))
            })?;
            if files
                .insert(relative.to_path_buf(), std::fs::read(&path)?)
                .is_some()
            {
                return Err(PackageError::Resolution(format!(
                    "cross-package file collision at {}",
                    relative.display()
                )));
            }
        } else {
            return Err(PackageError::HashMismatch(format!(
                "indexed package contains unsupported entry {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn collect_cached_files(
    base: &Path,
    dir: &Path,
    files: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(), PackageError> {
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|error| PackageError::HashMismatch(format!("cached site is unreadable: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PackageError::HashMismatch(format!(
            "cached site {} is not a real directory",
            dir.display()
        )));
    }
    for entry in std::fs::read_dir(dir)
        .map_err(|error| PackageError::HashMismatch(format!("cached site is unreadable: {error}")))?
    {
        let entry = entry.map_err(|error| PackageError::HashMismatch(error.to_string()))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| PackageError::HashMismatch(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::HashMismatch(format!(
                "cached site contains symbolic link {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_cached_files(base, &path, files)?;
        } else if metadata.is_file() {
            let relative = path.strip_prefix(base).map_err(|_| {
                PackageError::HashMismatch(format!("cached site path escaped {}", base.display()))
            })?;
            files.insert(
                relative.to_path_buf(),
                std::fs::read(&path).map_err(|error| {
                    PackageError::HashMismatch(format!(
                        "cached site file {} is unreadable: {error}",
                        path.display()
                    ))
                })?,
            );
        } else {
            return Err(PackageError::HashMismatch(format!(
                "cached site contains unsupported entry {}",
                path.display()
            )));
        }
    }
    Ok(())
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
        let metadata = std::fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::HashMismatch(format!(
                "indexed package contains symbolic link {}",
                from.display()
            )));
        }
        if metadata.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&from, &to)?;
        } else if std::fs::symlink_metadata(&to).is_ok() {
            return Err(PackageError::Resolution(format!(
                "cross-package file collision at {}",
                to.display()
            )));
        } else if metadata.is_file() {
            std::fs::copy(&from, &to)?;
        } else {
            return Err(PackageError::HashMismatch(format!(
                "indexed package contains unsupported entry {}",
                from.display()
            )));
        }
    }
    Ok(())
}
