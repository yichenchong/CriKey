//! The lockfile: resolution's durable, byte-stable record (spec 23.2).
//!
//! A lockfile is what resolution produces and reuse consumes. It must survive a
//! TOML round trip unchanged and serialise to identical bytes every time,
//! independent of the in-memory order of its packages — a wobbling lockfile
//! would make a content-addressed environment's identity wobble with it, so the
//! packages are canonicalised (sorted) on write.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::index::{is_hex_sha256, normalize_name};
use crate::PackageError;

/// One resolved, hash-pinned dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub hash: String,
}

/// Lockfile (spec 23.2): produced by resolution, consumed on reuse. TOML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lockfile {
    pub requires_python: String,
    pub packages: Vec<LockedPackage>,
}

const LOCKFILE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockfileDocument {
    #[serde(default)]
    format_version: Option<u32>,
    requires_python: String,
    #[serde(default, rename = "package")]
    packages: Vec<LockedPackage>,
}

impl Lockfile {
    /// Serialise to TOML with packages sorted so the byte output is independent
    /// of the in-memory ordering.
    pub fn to_toml(&self) -> String {
        let mut packages = self.packages.clone();
        packages.sort_by(|a, b| {
            (
                normalize_name(&a.name),
                &a.version,
                a.hash.to_ascii_lowercase(),
                &a.name,
            )
                .cmp(&(
                    normalize_name(&b.name),
                    &b.version,
                    b.hash.to_ascii_lowercase(),
                    &b.name,
                ))
        });
        let document = LockfileDocument {
            format_version: Some(LOCKFILE_FORMAT_VERSION),
            requires_python: self.requires_python.clone(),
            packages,
        };
        // A document this simple cannot fail to serialise to TOML.
        toml::to_string(&document).expect("a lockfile always serialises to TOML")
    }

    /// Validate fields that TOML's type checker cannot validate, including the
    /// mandatory SHA-256 digest on every package.
    pub(crate) fn validate(&self) -> Result<(), PackageError> {
        if self.requires_python.trim().is_empty() {
            return Err(PackageError::Resolution(
                "lockfile requires_python must not be empty".to_owned(),
            ));
        }
        let mut names = BTreeSet::new();
        for package in &self.packages {
            let name = normalize_name(&package.name);
            if name.is_empty() || package.version.trim().is_empty() {
                return Err(PackageError::Resolution(
                    "lockfile packages require non-empty name and version".to_owned(),
                ));
            }
            if !is_hex_sha256(&package.hash) {
                return Err(PackageError::HashMismatch(format!(
                    "lockfile package `{}` has a missing or invalid SHA-256 digest",
                    package.name
                )));
            }
            if !names.insert(name.clone()) {
                return Err(PackageError::Resolution(format!(
                    "lockfile contains more than one version of `{name}`"
                )));
            }
        }
        Ok(())
    }

    /// Parse a lockfile from TOML. Unknown fields and unsupported format
    /// versions are rejected rather than silently ignored.
    pub fn from_toml(s: &str) -> Result<Lockfile, PackageError> {
        let document: LockfileDocument =
            toml::from_str(s).map_err(|e| PackageError::Resolution(format!("malformed lockfile: {e}")))?;
        let format_version = document.format_version.unwrap_or(LOCKFILE_FORMAT_VERSION);
        if format_version != LOCKFILE_FORMAT_VERSION {
            return Err(PackageError::Resolution(format!(
                "unsupported lockfile format version {format_version}"
            )));
        }
        let lockfile = Lockfile {
            requires_python: document.requires_python,
            packages: document.packages,
        };
        lockfile.validate()?;
        Ok(lockfile)
    }
}
