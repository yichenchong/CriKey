//! An offline, deterministic package index (spec 15.3, 23.2).
//!
//! Layout: `<root>/<name>-<version>/`, each dir a "wheel" = a tree of importable
//! Python modules. A package's hash is the SHA-256 of the deterministic
//! (path-sorted) digest of that tree, so it is stable across independent loads
//! and sensitive to every byte of every module.

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
        let mut packages = Vec::new();
        // Sort directory entries so the load order is itself deterministic.
        let mut entries: Vec<PathBuf> = std::fs::read_dir(root)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();

        for path in entries {
            if !path.is_dir() {
                continue;
            }
            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            // `<name>-<version>`: split on the LAST hyphen so package names may
            // themselves contain hyphens while the version tail stays intact.
            let (name, version) = match dir_name.rsplit_once('-') {
                Some((n, v)) if !n.is_empty() && !v.is_empty() => (n.to_owned(), v.to_owned()),
                _ => continue,
            };
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
        let name = name.to_string();
        self.packages.iter().filter(move |p| p.name == name)
    }

    /// The indexed wheel matching an exact `(name, version)`, if present.
    pub(crate) fn get(&self, name: &str, version: &str) -> Option<&IndexedPackage> {
        self.packages
            .iter()
            .find(|p| p.name == name && p.version == version)
    }
}

/// The deterministic content hash of a module tree: SHA-256 over every file's
/// relative path and bytes, walked in path-sorted order.
fn tree_hash(root: &Path) -> Result<String, PackageError> {
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
        if path.is_dir() {
            collect_files(base, &path, out)?;
        } else {
            // A relative path with forward slashes: stable across platforms so
            // the same tree hashes identically everywhere.
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
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
