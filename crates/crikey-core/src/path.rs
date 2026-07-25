//! Lossless platform paths (spec 18.3).
//!
//! Path identity must survive non UTF-8 filesystem names; display formatting
//! is a separate concern and may be lossy.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlatformPath(OsString);

impl PlatformPath {
    pub fn new(path: impl Into<OsString>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        PathBuf::from(self.0)
    }

    /// Lossy, human readable rendering. Never used as identity.
    pub fn display(&self) -> std::path::Display<'_> {
        self.as_path().display()
    }
}

impl From<PathBuf> for PlatformPath {
    fn from(value: PathBuf) -> Self {
        Self(value.into_os_string())
    }
}
