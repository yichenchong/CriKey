//! An offline, deterministic package index (spec 15.3, 23.2).
//!
//! Layout: `<root>/<name>-<version>/`, each dir a "wheel" = a tree of importable
//! Python modules. A package's hash is the SHA-256 of the deterministic
//! (path-sorted) digest of that tree, so it is stable across independent loads
//! and sensitive to every byte of every module.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::PackageError;

/// One indexed wheel: its name, version, on-disk root and content hash.
#[derive(Debug, Clone)]
pub(crate) struct IndexedPackage {
    pub name: String,
    pub version: String,
    pub hash: String,
    pub root: PathBuf,
}

/// An offline, deterministic package index.
#[derive(Debug, Clone)]
pub struct PackageIndex {
    packages: Vec<IndexedPackage>,
}
impl PackageIndex {
    /// Load the index from a directory of `<name>-<version>/` wheel dirs.
    pub fn from_dir(root: &Path) -> Result<PackageIndex, PackageError> {
        let root_metadata = std::fs::symlink_metadata(root)?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(PackageError::MalformedIndex(format!(
                "package index root {} is not a real directory",
                root.display()
            )));
        }
        let mut packages = Vec::new();
        let mut identities = BTreeSet::new();
        // Sort directory entries so the load order is itself deterministic.
        let mut entries: Vec<PathBuf> = std::fs::read_dir(root)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<_, _>>()?;
        entries.sort();

        for path in entries {
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(PackageError::MalformedIndex(format!(
                    "symbolic links are not allowed in package indexes: {}",
                    path.display()
                )));
            }
            if !metadata.is_dir() {
                continue;
            }
            let dir_name = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
                PackageError::MalformedIndex(format!(
                    "package directory name {} is not valid UTF-8",
                    path.display()
                ))
            })?;
            // A pre-release can itself contain a hyphen (for example
            // `1.0.0-alpha`), so splitting at the last hyphen would mistake
            // `alpha` for the whole version. Select the first suffix that
            // has the shape of a version and therefore leave package-name
            // hyphens on the left.
            let (raw_name, version) = match split_index_dir_name(dir_name) {
                Some((name, version)) => (name, version),
                None => continue,
            };
            let name = normalize_name(raw_name);
            if name.is_empty() {
                continue;
            }
            if !identities.insert((name.clone(), version.clone())) {
                return Err(PackageError::Resolution(format!(
                    "duplicate indexed package `{name}=={version}` after name normalisation"
                )));
            }
            let hash = tree_hash(&path)?;
            packages.push(IndexedPackage {
                name,
                version,
                hash,
                root: path,
            });
        }

        Ok(PackageIndex { packages })
    }

    /// All indexed versions of `name`.
    pub(crate) fn versions<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a IndexedPackage> + 'a {
        let normalized = normalize_name(name);
        self.packages.iter().filter(move |p| p.name == normalized)
    }

    /// The indexed wheel matching an exact `(name, version)`, if present.
    pub(crate) fn get(&self, name: &str, version: &str) -> Option<&IndexedPackage> {
        let normalized = normalize_name(name);
        self.packages
            .iter()
            .find(|p| p.name == normalized && p.version == version)
    }
}

/// Split an on-disk `<name>-<version>` directory without losing a
/// hyphenated pre-release suffix such as `1.0.0-alpha`.
fn split_index_dir_name(dir_name: &str) -> Option<(&str, String)> {
    let mut search_from = 0;
    while let Some(relative) = dir_name[search_from..].find('-') {
        let split = search_from + relative;
        let version_start = split + 1;
        if split > 0 && version_start < dir_name.len() && looks_like_version(&dir_name[version_start..]) {
            return Some((&dir_name[..split], dir_name[version_start..].to_owned()));
        }
        search_from = version_start;
    }
    None
}

fn looks_like_version(version: &str) -> bool {
    let version = version.strip_prefix('v').unwrap_or(version);
    let Some(first) = version.as_bytes().first() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    if version.matches('-').count() > 1 {
        return false;
    }
    version
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'!' | b'-' | b'_'))
}

/// The deterministic content hash of a module tree: SHA-256 over every file's
/// relative path and bytes, walked in path-sorted order.
pub(crate) fn tree_hash(root: &Path) -> Result<String, PackageError> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, abs) in &files {
        let bytes = std::fs::read(abs)?;
        // Length-prefix path and content so no reshuffling of bytes across the
        // path/content boundary can collide two distinct trees.
        hasher.update((rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), PackageError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::MalformedIndex(format!(
                "symbolic links are not allowed in package indexes: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_files(base, &path, out)?;
        } else if metadata.is_file() {
            // A lossy conversion would silently drop or rewrite a component,
            // allowing two different trees to receive the same digest. Reject
            // paths that cannot be represented in the stable UTF-8 hash.
            let relative_path = path.strip_prefix(base).map_err(|_| {
                PackageError::MalformedIndex(format!(
                    "package path {} escaped index root {}",
                    path.display(),
                    base.display()
                ))
            })?;
            let mut components = Vec::new();
            for component in relative_path.components() {
                let component = component.as_os_str().to_str().ok_or_else(|| {
                    PackageError::MalformedIndex(format!(
                        "package path {} is not valid UTF-8",
                        path.display()
                    ))
                })?;
                components.push(component);
            }
            let rel = components.join("/");
            out.push((rel, path));
        } else {
            return Err(PackageError::MalformedIndex(format!(
                "unsupported package index entry: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Lowercase hex of a digest.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}
/// Canonical PEP 503 spelling for a package name.
pub(crate) fn normalize_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut pending_separator = false;
    for byte in name.bytes() {
        if matches!(byte, b'-' | b'_' | b'.') {
            pending_separator = true;
            continue;
        }
        if pending_separator && !normalized.is_empty() {
            normalized.push('-');
        }
        normalized.push(byte.to_ascii_lowercase() as char);
        pending_separator = false;
    }
    normalized
}

pub(crate) fn is_hex_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Compare two hexadecimal digests without returning early on a differing
/// byte. Callers validate the hexadecimal shape separately.
pub(crate) fn constant_time_hex_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (&left, &right) in left.as_bytes().iter().zip(right.as_bytes()) {
        difference |= left.to_ascii_lowercase() ^ right.to_ascii_lowercase();
    }
    difference == 0
}
