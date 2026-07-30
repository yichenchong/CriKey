//! The lockfile: resolution's durable, byte-stable record (spec 23.2).
//!
//! A lockfile is what resolution produces and reuse consumes. It must survive a
//! TOML round trip unchanged and serialise to identical bytes every time,
//! independent of the in-memory order of its packages — a wobbling lockfile
//! would make a content-addressed environment's identity wobble with it, so the
//! packages are canonicalised (sorted) on write.

use serde::{Deserialize, Serialize};

use crate::PackageError;

/// One resolved, hash-pinned dependency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub hash: String,
}

/// Lockfile (spec 23.2): produced by resolution, consumed on reuse. TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lockfile {
    pub requires_python: String,
    #[serde(default, rename = "package")]
    pub packages: Vec<LockedPackage>,
}

impl Lockfile {
    /// Serialise to TOML with packages sorted so the byte output is independent
    /// of the in-memory ordering.
    pub fn to_toml(&self) -> String {
        let mut canonical = self.clone();
        canonical
            .packages
            .sort_by(|a, b| (&a.name, &a.version, &a.hash).cmp(&(&b.name, &b.version, &b.hash)));
        // A struct this simple cannot fail to serialise to TOML.
        toml::to_string(&canonical).expect("a lockfile always serialises to TOML")
    }

    /// Parse a lockfile from TOML. Malformed input is a typed [`PackageError`],
    /// never a panic.
    pub fn from_toml(s: &str) -> Result<Lockfile, PackageError> {
        toml::from_str(s).map_err(|e| PackageError::Resolution(format!("malformed lockfile: {e}")))
    }
}
