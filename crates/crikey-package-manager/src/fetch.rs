//! Fetching a plugin package named by URL (spec 23.1).
//!
//! The network is behind a trait for one reason: a test that downloads
//! something is not a test of this crate. Every test in the workspace injects
//! a fake [`PackageFetcher`], and the real one is exercised only where it
//! cannot reach a socket — it refuses a non-HTTP URL before the client exists.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::PackageError;

/// Retrieves the bytes a URL names into a local file.
///
/// Implementations write `destination` in full or leave it unusable and return
/// an error; the installer treats a fetched file as a candidate archive and
/// never as an installation, so a partial download can only fail validation.
pub trait PackageFetcher {
    /// Fetches `url` into `destination`.
    fn fetch(&self, url: &str, destination: &Path) -> Result<(), PackageError>;
}

/// The production fetcher: one HTTPS GET into a file.
///
/// Deliberately not a general-purpose downloader. No resume, no cache, no
/// authentication — a plugin package is fetched once into a scratch file and
/// is then subject to exactly the same validation as a package that was
/// already on disk.
#[derive(Debug, Clone, Copy)]
pub struct HttpFetcher {
    max_bytes: u64,
}

/// Ceiling on a fetched package, matching the legacy loader's whole-package
/// cap. A launcher must not be able to fill a disk because a URL pointed at
/// something enormous, and every real plugin package is orders of magnitude
/// smaller.
const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

impl Default for HttpFetcher {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl HttpFetcher {
    /// A fetcher with the default size ceiling.
    pub fn new() -> Self {
        Self::default()
    }
}

impl PackageFetcher for HttpFetcher {
    fn fetch(&self, url: &str, destination: &Path) -> Result<(), PackageError> {
        // Checked before the client is touched: `file:` and `data:` URLs would
        // otherwise turn "download this package" into a local file read chosen
        // by whoever wrote the URL.
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(PackageError::SourceUnavailable(format!(
                "{url} is not an http or https URL"
            )));
        }

        let mut response = ureq::get(url).call().map_err(|error| {
            PackageError::SourceUnavailable(format!("{url} could not be fetched: {error}"))
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(PackageError::SourceUnavailable(format!(
                "{url} answered {status}"
            )));
        }

        let mut file = File::create(destination)?;
        // One byte past the ceiling, so an over-long body is detected rather
        // than silently truncated into a plausible-looking archive.
        let mut reader = response.body_mut().as_reader().take(self.max_bytes + 1);
        let written = io::copy(&mut reader, &mut file)?;
        if written > self.max_bytes {
            return Err(PackageError::SourceUnavailable(format!(
                "{url} is larger than the {} byte package limit",
                self.max_bytes
            )));
        }
        file.flush()?;
        Ok(())
    }
}
