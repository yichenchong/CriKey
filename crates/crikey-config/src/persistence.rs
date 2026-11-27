//! Blocking configuration persistence. Call only from CLI/configuration workers.
//!
//! Every cooperative writer locks the same persistent inode. The lock file is
//! never unlinked: OS locks release on close/process exit without stale-file
//! recovery or polling. External editors do not cooperate; exact source checks
//! immediately before replacement detect observed edits, but cannot prevent an
//! editor racing the final check/rename. Publication always uses a full reload.
use crate::ConfigError;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Exact source contents/identity plus session overrides, not a process counter.
/// Debug deliberately reveals neither source contents nor session values.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConfigRevision {
    pub(crate) sources: BTreeMap<PathBuf, SourceState>,
    pub(crate) session: BTreeMap<String, String>,
}
impl std::fmt::Debug for ConfigRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigRevision")
            .field("sources", &self.sources.len())
            .finish_non_exhaustive()
    }
}
impl ConfigRevision {
    pub(crate) fn check(&self) -> Result<(), ConfigError> {
        for (path, expected) in &self.sources {
            if &SourceState::read(path)? != expected {
                return Err(ConfigError::Conflict);
            }
        }
        Ok(())
    }
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SourceState {
    pub(crate) bytes: Option<Vec<u8>>,
    pub(crate) is_directory: bool,
    entries: Vec<PathBuf>,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: Option<(u64, u64)>,
}
impl SourceState {
    pub(crate) fn read(path: &Path) -> Result<Self, ConfigError> {
        let read = || -> std::io::Result<Self> {
            let metadata = match fs::metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Self {
                        bytes: None,
                        is_directory: false,
                        entries: Vec::new(),
                        modified: None,
                        created: None,
                        #[cfg(unix)]
                        identity: None,
                    })
                }
                Err(error) => return Err(error),
            };
            let mut bytes = None;
            let mut entries = Vec::new();
            if metadata.is_dir() {
                for entry in fs::read_dir(path)? {
                    let path = entry?.path();
                    if path.extension().is_some_and(|extension| extension == "toml") {
                        entries.push(path);
                    }
                }
                entries.sort();
            } else {
                let mut file = File::open(path)?;
                let mut content = Vec::new();
                file.read_to_end(&mut content)?;
                if file.metadata()?.modified().ok() != metadata.modified().ok() {
                    return Err(std::io::Error::other("configuration changed while reading"));
                }
                bytes = Some(content);
            }
            Ok(Self {
                bytes,
                is_directory: metadata.is_dir(),
                entries,
                modified: metadata.modified().ok(),
                created: metadata.created().ok(),
                #[cfg(unix)]
                identity: {
                    use std::os::unix::fs::MetadataExt;
                    Some((metadata.dev(), metadata.ino()))
                },
            })
        };
        read().map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })
    }
}

pub(crate) fn writer_lock(path: &Path) -> Result<File, ConfigError> {
    let parent = path.parent().expect("configuration has a parent directory");
    let acquire = || -> std::io::Result<File> {
        fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(parent.join(".configuration-writer.lock"))?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    };
    acquire().map_err(|source| ConfigError::Write {
        path: parent.to_path_buf(),
        source,
    })
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8], expected: &ConfigRevision) -> Result<(), ConfigError> {
    let prepare = || -> std::io::Result<tempfile::NamedTempFile> {
        let parent = path.parent().expect("configuration has a parent directory");
        let mut staging = tempfile::NamedTempFile::new_in(parent)?;
        match fs::metadata(path) {
            Ok(metadata) => staging.as_file().set_permissions(metadata.permissions())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        staging.write_all(bytes)?;
        staging.as_file().sync_all()?;
        Ok(staging)
    };
    let staging = prepare().map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    expected.check()?;
    staging.persist(path).map_err(|error| ConfigError::Write {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}
