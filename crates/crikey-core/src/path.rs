//! Lossless platform paths (spec 18.3).
//!
//! Path identity must survive non UTF-8 filesystem names; display formatting
//! is a separate concern and may be lossy.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[cfg(all(unix, test))]
use std::os::unix::ffi::OsStringExt;

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

impl From<OsString> for PlatformPath {
    fn from(value: OsString) -> Self {
        Self(value)
    }
}

impl From<&OsStr> for PlatformPath {
    fn from(value: &OsStr) -> Self {
        Self(value.to_owned())
    }
}

impl From<&Path> for PlatformPath {
    fn from(value: &Path) -> Self {
        Self(value.as_os_str().to_owned())
    }
}

impl AsRef<Path> for PlatformPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl AsRef<OsStr> for PlatformPath {
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_conversions_preserve_the_native_value() {
        let source = Path::new("folder").join("entry");
        let platform = PlatformPath::from(source.as_path());

        assert_eq!(platform.as_path(), source);
        assert_eq!(<PlatformPath as AsRef<Path>>::as_ref(&platform), source);
        assert_eq!(
            <PlatformPath as AsRef<OsStr>>::as_ref(&platform),
            source.as_os_str()
        );
        assert_eq!(platform.clone().into_path_buf(), source);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_os_paths_round_trip_without_loss() {
        let original = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff, b'x']);
        let platform = PlatformPath::from(original.clone());

        assert_eq!(platform.as_os_str(), original.as_os_str());
        assert_eq!(platform.into_path_buf().into_os_string(), original);
    }
}
