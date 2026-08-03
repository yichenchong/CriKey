//! Legacy package discovery and loading (spec 14.3; acceptance 31.11).
//!
//! Spec 14.3 requires CriKey to load Keypirinha packages in both forms that
//! exist in the wild: a loose directory, and a `.keypirinha-package` ZIP
//! archive whose entries are package-root-relative with no wrapping folder.
//! [`PackageLoader`] turns either into the same [`LegacyPackage`]: an id, the
//! root it came from, the importable package-local modules, and the resources.
//!
//! # Identity, and why nothing is canonicalized
//!
//! The package id is the directory name, or the archive name with
//! `.keypirinha-package` removed — verbatim, no case folding. The loose and the
//! zipped form of one package therefore carry the same id, which is what lets a
//! user replace one with the other without every setting keyed on the id being
//! orphaned.
//!
//! [`LegacyPackage::root`] reports exactly the path the caller supplied.
//! Resolving symlinks or `..` here would make a compatibility diagnostic (spec
//! 26.2) name a path the user has never seen, and on Windows it would rewrite a
//! mapped drive into a UNC path. Canonicalization is a security tool for the
//! *contents* of an archive, not for the root the user pointed at, and it is
//! applied there instead — see [`PackageError::UnsafeEntryPath`].
//!
//! # Modules and resources
//!
//! Every `.py` file is an importable package-local module named by its path:
//! `lib/helpers.py` is `lib.helpers`, and `lib/__init__.py` is the package
//! `lib`. Everything else — icons, data files, the Keypirinha-style `.ini`
//! configuration read by [`crate::config`] — is a resource. The two sets are
//! disjoint, sorted and duplicate-free, because a plugin's import set must not
//! depend on `read_dir` order.
//!
//! The main plugin module is the top-level module whose stem equals the package
//! id, else the lexicographically first top-level module. A package with no
//! top-level module at all is not an empty package, it is a broken one, and it
//! is refused by name ([`PackageError::NoPluginModule`]).
//!
//! # Extraction is a cache concern
//!
//! An archive extracts under [`PackageLoader::cache_root`] into a
//! *content-addressed* directory, `<id>-<digest>`. That single decision buys
//! three properties at once: extraction is deterministic (the same archive
//! always yields the same path), idempotent (a present directory is reused
//! whole), and never stale (an archive rewritten in place digests differently,
//! so the old extraction cannot answer for the new bytes). Extraction is staged
//! in a hidden sibling directory and moved into place with a single rename, so
//! a directory bearing a content-addressed name is always complete.
//!
//! # Refusal is the security surface
//!
//! An archive is hostile input: it arrives from the internet and its entry
//! names are attacker-controlled. Every entry name in the central directory is
//! decoded, checked for escapes and checked against the size caps *before the
//! first byte is written*, and the classification that decides whether the
//! package is even loadable runs before extraction too. A loader that extracts
//! as it scans has already lost by the time it notices, and a zip bomb only has
//! to be noticed late once to fill the disk.
//!
//! # Bounds
//!
//! [`PackageLimits`] caps per-entry bytes, whole-package bytes, entry count and
//! packages per root; each overflow is a refusal or a documented stop, never a
//! truncation. Directory walks never follow symlinks, so a link loop cannot
//! make the walk diverge, and the extraction cache keeps at most one directory
//! per package id.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zip::ZipArchive;

/// File extension of a zipped legacy package, without the dot (spec 14.3).
pub const PACKAGE_ARCHIVE_EXTENSION: &str = "keypirinha-package";

/// Extension of an importable package-local module.
///
/// Matched case-sensitively on purpose: CPython's path finder matches the
/// `.py` suffix against the directory listing byte for byte, so `Plugin.PY` is
/// not importable even on a case-insensitive filesystem, and calling it a
/// module would promise an import that fails at runtime.
const PYTHON_EXTENSION: &str = "py";

/// Name of the file that makes a directory a Python package rather than a
/// plain namespace directory.
const PACKAGE_INIT_FILE: &str = "__init__.py";

/// Width of the hex content digest in an extraction directory name. Fixed so a
/// cache sweep can tell `<id>-<digest>` from an unrelated package whose id
/// merely starts with `<id>-`.
const DIGEST_HEX_WIDTH: usize = 16;

/// Copy buffer for extraction. One buffer is allocated per extracted archive
/// and reused across its entries.
const EXTRACTION_BUFFER_BYTES: usize = 64 * 1024;

/// Distinguishes concurrent staging and trash directories within one process.
/// Not a clock: extraction must not depend on wall time (spec 14.8).
static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// Identity of a legacy package: the directory name, or the archive name with
/// the `.keypirinha-package` extension removed, verbatim.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId(pub String);

impl PackageId {
    /// Borrows the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One importable package-local Python module.
///
/// Field order is load-bearing: the derived ordering sorts by import name,
/// which is the order [`LegacyPackage::modules`] is reported in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageModule {
    /// Dotted import name, relative to the package root: `lib.helpers`.
    pub import_name: String,
    /// Package-relative path of the file, resolved against
    /// [`PackageRoot::content_root`].
    pub relative_path: PathBuf,
}

/// Where a package's files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageRoot {
    /// A loose package directory, exactly as the caller named it.
    Directory(PathBuf),
    /// A `.keypirinha-package` archive and the cache directory it was
    /// extracted into.
    Archive {
        /// The archive as the caller named it, kept for diagnostics.
        archive: PathBuf,
        /// Extraction directory under [`PackageLoader::cache_root`].
        extracted: PathBuf,
    },
}

impl PackageRoot {
    /// Directory that package-relative module and resource paths resolve
    /// against.
    pub fn content_root(&self) -> &Path {
        match self {
            Self::Directory(directory) => directory,
            Self::Archive { extracted, .. } => extracted,
        }
    }
}

/// A loaded legacy package (spec 14.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyPackage {
    /// Package identity.
    pub id: PackageId,
    /// Where the package was loaded from.
    pub root: PackageRoot,
    /// Import name of the plugin entry point. Always also present in
    /// [`Self::modules`].
    pub main_module: String,
    /// Importable package-local modules, sorted by import name, unique.
    pub modules: Vec<PackageModule>,
    /// Package-relative resource paths, sorted, unique, never `.py`.
    pub resources: Vec<PathBuf>,
}

/// Caps applied to everything a package can make the loader hold or write.
///
/// A legacy package is third-party content of unknown provenance, so every
/// bound is explicit and every overflow is a refusal — the loader never
/// truncates an entry, because half a Python module that still parses is worse
/// than no module at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageLimits {
    /// Largest single entry, in uncompressed bytes. Exceeded (declared *or*
    /// actual) yields [`PackageError::EntryTooLarge`] and writes nothing.
    pub max_entry_bytes: u64,
    /// Largest whole package, in uncompressed bytes. Exceeded yields
    /// [`PackageError::PackageTooLarge`].
    pub max_total_bytes: u64,
    /// Most entries one package may contain. Exceeded yields
    /// [`PackageError::TooManyEntries`]; for a loose directory the walk is
    /// abandoned as soon as the cap is passed, so a symlink loop costs a
    /// bounded number of `read_dir` calls rather than diverging.
    pub max_entries: usize,
    /// Most package candidates [`PackageLoader::discover`] considers in one
    /// root. Further entries in that root are ignored; the scan itself never
    /// fails, so a pathological directory degrades discovery instead of
    /// hanging the launcher.
    pub max_packages_per_root: usize,
}

impl Default for PackageLimits {
    fn default() -> Self {
        // Generous enough that no plausible real package trips a cap — a
        // Keypirinha package bundling a vendored library is a few hundred
        // small files — and tight enough that a hostile archive cannot fill a
        // disk before the first refusal.
        Self {
            max_entry_bytes: 32 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_entries: 4_096,
            max_packages_per_root: 1_024,
        }
    }
}

/// Why a package could not be loaded.
///
/// Every variant names the artefact it refused — archive, package, entry — so
/// the compatibility report (spec 26.2) can point the user at something they
/// recognize. A refusal nobody can act on is barely better than a silent one.
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    /// The path is neither a directory nor a `.keypirinha-package` archive.
    #[error("`{}` is not a legacy package: expected a directory or a `.{}` archive", path.display(), PACKAGE_ARCHIVE_EXTENSION)]
    NotAPackage {
        /// The path that was offered.
        path: PathBuf,
    },

    /// The package holds no top-level `.py` module, so it has no plugin entry
    /// point.
    #[error("legacy package `{package}` has no top-level plugin module in `{}`", root.display())]
    NoPluginModule {
        /// Id derived from the path.
        package: PackageId,
        /// The path the caller pointed at.
        root: PathBuf,
    },

    /// The archive could not be opened as a ZIP container.
    #[error("legacy package archive `{}` is malformed: {detail}", archive.display())]
    MalformedArchive {
        /// The archive as the caller named it.
        archive: PathBuf,
        /// Reader diagnostic, never empty.
        detail: String,
    },

    /// The archive contains no entries at all.
    #[error("legacy package archive `{}` is empty", archive.display())]
    EmptyArchive {
        /// The archive as the caller named it.
        archive: PathBuf,
    },

    /// An entry is a symbolic link rather than a regular file.
    #[error("legacy package archive `{}` contains symbolic-link entry `{entry}`", archive.display())]
    SymlinkEntry {
        /// The archive as the caller named it.
        archive: PathBuf,
        /// The offending entry name, exactly as the archive declared it.
        entry: String,
    },

    /// Two entries resolve to the same package-relative path.
    #[error("legacy package archive `{}` contains duplicate entry name `{entry}`", archive.display())]
    DuplicateEntryName {
        /// The archive as the caller named it.
        archive: PathBuf,
        /// The duplicate name, normalized to the package-relative path.
        entry: String,
    },

    /// An entry names a path outside the package root, or one that cannot be
    /// written safely on every supported platform.
    #[error("legacy package archive `{}` contains an entry outside the package root: `{entry}`", archive.display())]
    UnsafeEntryPath {
        /// The archive as the caller named it.
        archive: PathBuf,
        /// The offending entry name, exactly as the archive declared it.
        entry: String,
    },

    /// An entry name is not valid UTF-8 and will not be lossily decoded into a
    /// path.
    #[error("legacy package archive `{}` contains an entry name that is not valid UTF-8: `{}`", archive.display(), escape_bytes(raw_name))]
    NonUtf8EntryName {
        /// The archive as the caller named it.
        archive: PathBuf,
        /// The raw entry name bytes, preserved exactly.
        raw_name: Vec<u8>,
    },

    /// An entry is larger than [`PackageLimits::max_entry_bytes`], or larger
    /// than the size it declared.
    #[error("legacy package archive `{}` entry `{entry}` is {size} bytes, over its {limit} byte limit", archive.display())]
    EntryTooLarge {
        /// The archive as the caller named it.
        archive: PathBuf,
        /// The offending entry.
        entry: String,
        /// Size that was refused.
        size: u64,
        /// Bound that was exceeded.
        limit: u64,
    },

    /// The package's entries total more than [`PackageLimits::max_total_bytes`].
    #[error("legacy package archive `{}` holds {size} bytes, over the {limit} byte package limit", archive.display())]
    PackageTooLarge {
        /// The archive as the caller named it.
        archive: PathBuf,
        /// Total that was refused.
        size: u64,
        /// Bound that was exceeded.
        limit: u64,
    },

    /// The package holds more than [`PackageLimits::max_entries`] entries.
    #[error("legacy package `{}` holds at least {count} entries, over the {limit} entry limit", path.display())]
    TooManyEntries {
        /// The package as the caller named it.
        path: PathBuf,
        /// Entries counted before the scan was abandoned.
        count: usize,
        /// Bound that was exceeded.
        limit: usize,
    },

    /// The filesystem refused an operation the loader needs.
    #[error("legacy package path `{}` could not be read: {error}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying failure. Rendered into the message as well as
        /// exposed as the source, because the compatibility report renders one
        /// line per finding and would otherwise drop the only actionable part.
        #[source]
        error: io::Error,
    },
}

/// Loads legacy packages, extracting archives under a cache root.
#[derive(Debug, Clone)]
pub struct PackageLoader {
    cache_root: PathBuf,
    limits: PackageLimits,
}

impl PackageLoader {
    /// A loader extracting under `cache_root`, with default [`PackageLimits`].
    ///
    /// The cache root is not created here: a loader that never meets an
    /// archive must not leave a directory behind, and discovery over a
    /// read-only root must not fail on a side effect nobody asked for.
    pub fn new(cache_root: PathBuf) -> Self {
        Self {
            cache_root,
            limits: PackageLimits::default(),
        }
    }

    /// A loader with explicit caps.
    pub fn with_limits(cache_root: PathBuf, limits: PackageLimits) -> Self {
        Self { cache_root, limits }
    }

    /// Directory archives are extracted under.
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Caps this loader enforces.
    pub fn limits(&self) -> PackageLimits {
        self.limits
    }

    /// Scans `roots` in order for legacy packages.
    ///
    /// Packages are grouped by root in argument order and sorted by id within
    /// a root, so precedence is the caller's list and never the alphabet: the
    /// first root wins a duplicated id and the shadowed package is dropped
    /// rather than reported twice.
    ///
    /// Discovery is best effort by design. A root that is missing, unreadable
    /// or not a directory is skipped, and so is a package that fails to load —
    /// one broken package must not cost the user every other plugin. The
    /// refusal itself is not lost: [`Self::load`] reports it when the user asks
    /// about that package specifically, which is where a diagnostic can be
    /// acted on.
    pub fn discover(&self, roots: &[PathBuf]) -> Result<Vec<LegacyPackage>, PackageError> {
        let mut packages: Vec<LegacyPackage> = Vec::new();
        let mut claimed: BTreeSet<PackageId> = BTreeSet::new();

        for root in roots {
            let Ok(entries) = fs::read_dir(root) else {
                continue;
            };

            let mut candidates: Vec<PathBuf> = Vec::new();
            for entry in entries.flatten() {
                if candidates.len() >= self.limits.max_packages_per_root {
                    break;
                }
                let path = entry.path();
                // `metadata` rather than `entry.file_type`: a package the user
                // symlinked into a root is a package, and following one link
                // named by the user cannot loop.
                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                if metadata.is_dir() || (metadata.is_file() && has_archive_extension(&path)) {
                    candidates.push(path);
                }
            }
            // Sorted before loading so that a root holding both `foo` and
            // `foo.keypirinha-package` resolves the same way every run; the
            // loose directory sorts first and wins, which is what a developer
            // dropping an unzipped copy next to the shipped archive expects.
            candidates.sort();

            let mut loaded: Vec<LegacyPackage> = candidates
                .iter()
                .filter_map(|candidate| self.load(candidate).ok())
                .collect();
            loaded.sort_by(|left, right| left.id.cmp(&right.id));

            for package in loaded {
                if claimed.contains(&package.id) {
                    continue;
                }
                claimed.insert(package.id.clone());
                packages.push(package);
            }
        }

        Ok(packages)
    }

    /// Loads the package at `path`, which is a package directory or a
    /// `.keypirinha-package` archive.
    pub fn load(&self, path: &Path) -> Result<LegacyPackage, PackageError> {
        if has_archive_extension(path) && path.is_file() {
            self.load_archive(path)
        } else if path.is_dir() {
            self.load_directory(path)
        } else {
            Err(PackageError::NotAPackage {
                path: path.to_path_buf(),
            })
        }
    }

    fn load_directory(&self, path: &Path) -> Result<LegacyPackage, PackageError> {
        let id = package_id_of(path).ok_or_else(|| PackageError::NotAPackage {
            path: path.to_path_buf(),
        })?;
        let files = self.scan_directory(path)?;
        let contents = package_contents(&id, path, files)?;

        Ok(LegacyPackage {
            id: PackageId(id),
            root: PackageRoot::Directory(path.to_path_buf()),
            main_module: contents.main_module,
            modules: contents.modules,
            resources: contents.resources,
        })
    }

    /// Package-relative paths of every regular file below `root`.
    ///
    /// Symlinks are neither followed nor reported. A link in a package
    /// directory can point anywhere, and the loader's contract is that every
    /// path it reports resolves under [`PackageRoot::content_root`]; refusing
    /// to walk them also makes a link loop impossible rather than merely
    /// bounded.
    fn scan_directory(&self, root: &Path) -> Result<Vec<PathBuf>, PackageError> {
        let mut pending: Vec<PathBuf> = vec![PathBuf::new()];
        let mut files: Vec<PathBuf> = Vec::new();
        let mut seen = 0usize;

        while let Some(relative_directory) = pending.pop() {
            let absolute = root.join(&relative_directory);
            let entries = fs::read_dir(&absolute).map_err(|error| PackageError::Io {
                path: absolute.clone(),
                error,
            })?;

            for entry in entries {
                let entry = entry.map_err(|error| PackageError::Io {
                    path: absolute.clone(),
                    error,
                })?;
                let file_type = entry.file_type().map_err(|error| PackageError::Io {
                    path: entry.path(),
                    error,
                })?;

                seen += 1;
                if seen > self.limits.max_entries {
                    return Err(PackageError::TooManyEntries {
                        path: root.to_path_buf(),
                        count: seen,
                        limit: self.limits.max_entries,
                    });
                }

                let relative = relative_directory.join(entry.file_name());
                if file_type.is_dir() {
                    pending.push(relative);
                } else if file_type.is_file() {
                    files.push(relative);
                }
            }
        }

        Ok(files)
    }

    fn load_archive(&self, path: &Path) -> Result<LegacyPackage, PackageError> {
        let id = package_id_of(path).ok_or_else(|| PackageError::NotAPackage {
            path: path.to_path_buf(),
        })?;
        preflight_archive(path, self.limits.max_entries)?;
        let file = File::open(path).map_err(|error| PackageError::Io {
            path: path.to_path_buf(),
            error,
        })?;
        let mut archive =
            ZipArchive::new(BufReader::new(file)).map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;

        if archive.len() > self.limits.max_entries {
            return Err(PackageError::TooManyEntries {
                path: path.to_path_buf(),
                count: archive.len(),
                limit: self.limits.max_entries,
            });
        }
        if archive.is_empty() {
            return Err(PackageError::EmptyArchive {
                archive: path.to_path_buf(),
            });
        }

        let plan = self.plan_archive(path, &mut archive)?;
        // Classification before extraction: an archive that is not a loadable
        // package must not leave a directory in the cache, and the caller gets
        // the same refusal whether or not the cache already holds it.
        let contents = package_contents(&id, path, plan.iter().map(|entry| entry.relative.clone()))?;

        let digest = content_digest(path).map_err(|error| PackageError::Io {
            path: path.to_path_buf(),
            error,
        })?;
        let directory = format!("{id}-{digest:0width$x}", width = DIGEST_HEX_WIDTH);
        let extracted = self.cache_root.join(&directory);
        if !is_real_directory(&extracted) {
            self.extract(path, &mut archive, &plan, &extracted)?;
            self.sweep_superseded(&id, &directory);
        }

        Ok(LegacyPackage {
            id: PackageId(id),
            root: PackageRoot::Archive {
                archive: path.to_path_buf(),
                extracted,
            },
            main_module: contents.main_module,
            modules: contents.modules,
            resources: contents.resources,
        })
    }

    /// Validates every entry in the central directory and returns the file
    /// entries worth extracting.
    ///
    /// Nothing is written here. Every refusal below therefore happens with an
    /// untouched cache root, which is the whole point: an archive gets to be
    /// hostile exactly once, in metadata, before it can cost a byte of disk.
    /// Entries are visited in archive order so the reported refusal is the
    /// first offending entry, not whichever one a hash map happened to yield.
    fn plan_archive(
        &self,
        path: &Path,
        archive: &mut ZipArchive<ArchiveReader>,
    ) -> Result<Vec<PlannedEntry>, PackageError> {
        let mut planned: Vec<PlannedEntry> = Vec::new();
        let mut seen_paths = BTreeSet::new();
        let mut total = 0u64;

        for index in 0..archive.len() {
            let entry = archive
                .by_index_raw(index)
                .map_err(|error| PackageError::MalformedArchive {
                    archive: path.to_path_buf(),
                    detail: error.to_string(),
                })?;

            // `name_raw`, never `name`: the reader replaces an undecodable
            // byte with U+FFFD, and extracting under a name the archive did
            // not declare is a silent corruption. CPython's `zipfile` raises
            // on this same archive, so refusing keeps CriKey in line with the
            // reference reader instead of inventing a third behaviour.
            let raw_name = entry.name_raw();
            let name = std::str::from_utf8(raw_name).map_err(|_| PackageError::NonUtf8EntryName {
                archive: path.to_path_buf(),
                raw_name: raw_name.to_vec(),
            })?;

            let Some(relative) = safe_relative_path(name) else {
                return Err(PackageError::UnsafeEntryPath {
                    archive: path.to_path_buf(),
                    entry: name.to_owned(),
                });
            };
            if !seen_paths.insert(relative.clone()) {
                return Err(PackageError::DuplicateEntryName {
                    archive: path.to_path_buf(),
                    entry: relative.display().to_string(),
                });
            }

            // A symlink entry would be materialized as a regular file holding
            // its target, which is harmless but meaningless; refusing keeps
            // the invariant that every reported path is a real file under the
            // content root. Check this before ignoring directory entries so a
            // symlink cannot hide behind a trailing slash.
            if entry.is_symlink() {
                return Err(PackageError::SymlinkEntry {
                    archive: path.to_path_buf(),
                    entry: name.to_owned(),
                });
            }

            // Directory entries carry no content and are not resources; their
            // parents are created on demand for the files that need them. They
            // are still validated above, because an archive that so much as
            // declares `../` is one CriKey wants nothing to do with.
            if is_directory_entry(name) {
                continue;
            }

            let declared_size = entry.size();
            if declared_size > self.limits.max_entry_bytes {
                return Err(PackageError::EntryTooLarge {
                    archive: path.to_path_buf(),
                    entry: name.to_owned(),
                    size: declared_size,
                    limit: self.limits.max_entry_bytes,
                });
            }
            total = total.saturating_add(declared_size);
            if total > self.limits.max_total_bytes {
                return Err(PackageError::PackageTooLarge {
                    archive: path.to_path_buf(),
                    size: total,
                    limit: self.limits.max_total_bytes,
                });
            }

            planned.push(PlannedEntry {
                index,
                relative,
                declared_size,
            });
        }

        Ok(planned)
    }

    /// Materializes `plan` at `extracted`, atomically.
    ///
    /// Entries land in a hidden staging directory that is renamed into place
    /// once every one of them is written. A directory bearing a
    /// content-addressed name is therefore always a complete extraction, so
    /// [`Self::load_archive`] can treat its mere existence as proof and skip
    /// the work.
    fn extract(
        &self,
        path: &Path,
        archive: &mut ZipArchive<ArchiveReader>,
        plan: &[PlannedEntry],
        extracted: &Path,
    ) -> Result<(), PackageError> {
        fs::create_dir_all(&self.cache_root).map_err(|error| PackageError::Io {
            path: self.cache_root.clone(),
            error,
        })?;

        let staging = self.cache_root.join(scratch_name("staging"));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).map_err(|error| PackageError::Io {
            path: staging.clone(),
            error,
        })?;

        if let Err(error) = self.write_entries(path, archive, plan, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }

        if let Ok(metadata) = fs::symlink_metadata(extracted) {
            let file_type = metadata.file_type();
            if file_type.is_symlink() || file_type.is_file() {
                if let Err(error) = fs::remove_file(extracted) {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(PackageError::Io {
                        path: extracted.to_path_buf(),
                        error,
                    });
                }
            }
        }

        match fs::rename(&staging, extracted) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                // Losing a race is not a failure: the directory name is the
                // archive's content digest, so whoever won wrote these bytes.
                if is_real_directory(extracted) {
                    Ok(())
                } else {
                    Err(PackageError::Io {
                        path: extracted.to_path_buf(),
                        error,
                    })
                }
            }
        }
    }

    fn write_entries(
        &self,
        path: &Path,
        archive: &mut ZipArchive<ArchiveReader>,
        plan: &[PlannedEntry],
        staging: &Path,
    ) -> Result<(), PackageError> {
        let mut buffer = vec![0u8; EXTRACTION_BUFFER_BYTES];
        let mut total = 0u64;

        for planned in plan {
            let mut entry =
                archive
                    .by_index(planned.index)
                    .map_err(|error| PackageError::MalformedArchive {
                        archive: path.to_path_buf(),
                        detail: error.to_string(),
                    })?;

            let destination = staging.join(&planned.relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| PackageError::Io {
                    path: parent.to_path_buf(),
                    error,
                })?;
            }
            let mut sink = File::create(&destination).map_err(|error| PackageError::Io {
                path: destination.clone(),
                error,
            })?;

            let written = copy_bounded(&mut entry, &mut sink, planned.declared_size, &mut buffer).map_err(
                |failure| match failure {
                    // A read failure here is the archive's fault, not the
                    // filesystem's: it is a truncated member or a CRC the
                    // reader rejected at end of stream.
                    CopyFailure::Read(error) => PackageError::MalformedArchive {
                        archive: path.to_path_buf(),
                        detail: format!(
                            "entry `{}` could not be decompressed: {error}",
                            planned.relative.display()
                        ),
                    },
                    CopyFailure::Write(error) => PackageError::Io {
                        path: destination.clone(),
                        error,
                    },
                    // The header under-reported the entry. The declared size
                    // is the bound that was broken, and it was broken before
                    // the excess reached the disk.
                    CopyFailure::Oversized(size) => PackageError::EntryTooLarge {
                        archive: path.to_path_buf(),
                        entry: planned.relative.to_string_lossy().into_owned(),
                        size,
                        limit: planned.declared_size,
                    },
                },
            )?;

            total = total.saturating_add(written);
            if total > self.limits.max_total_bytes {
                return Err(PackageError::PackageTooLarge {
                    archive: path.to_path_buf(),
                    size: total,
                    limit: self.limits.max_total_bytes,
                });
            }
        }

        Ok(())
    }

    /// Drops extractions of earlier revisions of the same package.
    ///
    /// Without this, rewriting one archive in place leaks a full copy of it
    /// per revision, and a package updated weekly would own the cache. The
    /// doomed directory is renamed out of the content-addressed namespace
    /// before it is deleted, so a process killed mid-delete cannot leave a
    /// half-empty directory that a later load would mistake for a complete
    /// extraction. Failures are ignored: a directory that cannot be swept is
    /// wasted space, not a reason to refuse a package that loaded.
    fn sweep_superseded(&self, id: &str, keep: &str) {
        let Ok(entries) = fs::read_dir(&self.cache_root) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name == keep || !is_extraction_directory(&name, id) {
                continue;
            }
            let doomed = self.cache_root.join(scratch_name("trash"));
            if fs::rename(entry.path(), &doomed).is_ok() {
                let _ = fs::remove_dir_all(&doomed);
            }
        }
    }
}

/// Checks central-directory names before the ZIP reader indexes them.
///
/// The `zip` reader keeps only one record when a central directory repeats an
/// exact name, so this bounded pass rejects a duplicate before that loss.
fn preflight_archive(path: &Path, max_entries: usize) -> Result<(), PackageError> {
    let mut file = File::open(path).map_err(|error| PackageError::Io {
        path: path.to_path_buf(),
        error,
    })?;
    let length = file
        .metadata()
        .map_err(|error| PackageError::Io {
            path: path.to_path_buf(),
            error,
        })?
        .len();
    let tail_length = length.min(22 + u16::MAX as u64) as usize;
    let mut tail = vec![0; tail_length];
    file.seek(SeekFrom::Start(length - tail_length as u64))
        .and_then(|_| file.read_exact(&mut tail))
        .map_err(|error| PackageError::MalformedArchive {
            archive: path.to_path_buf(),
            detail: error.to_string(),
        })?;
    let eocd = tail
        .windows(4)
        .enumerate()
        .rev()
        .find_map(|(offset, sig)| {
            if sig != [0x50, 0x4b, 0x05, 0x06] || offset + 22 > tail.len() {
                return None;
            }
            let comment = u16::from_le_bytes([tail[offset + 20], tail[offset + 21]]) as usize;
            (offset + 22 + comment <= tail.len()).then_some(offset)
        })
        .ok_or_else(|| PackageError::MalformedArchive {
            archive: path.to_path_buf(),
            detail: "end-of-central-directory record was not found".to_owned(),
        })?;
    let eocd_absolute = length - tail_length as u64 + eocd as u64;
    let disk = u16::from_le_bytes([tail[eocd + 4], tail[eocd + 5]]);
    let central_disk = u16::from_le_bytes([tail[eocd + 6], tail[eocd + 7]]);
    let entries_on_disk_16 = u16::from_le_bytes([tail[eocd + 8], tail[eocd + 9]]);
    let entries_16 = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]);
    let central_size_32 =
        u32::from_le_bytes([tail[eocd + 12], tail[eocd + 13], tail[eocd + 14], tail[eocd + 15]]);
    let central_offset_32 =
        u32::from_le_bytes([tail[eocd + 16], tail[eocd + 17], tail[eocd + 18], tail[eocd + 19]]);
    let (entries, central_size, central_offset, central_record) = if entries_on_disk_16 == u16::MAX
        || entries_16 == u16::MAX
        || central_size_32 == u32::MAX
        || central_offset_32 == u32::MAX
    {
        let locator = eocd_absolute
            .checked_sub(20)
            .ok_or_else(|| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "ZIP64 locator is missing".to_owned(),
            })?;
        file.seek(SeekFrom::Start(locator))
            .map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;
        let mut locator_bytes = [0u8; 20];
        file.read_exact(&mut locator_bytes)
            .map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;
        if locator_bytes[..4] != [0x50, 0x4b, 0x06, 0x07] {
            return Err(PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "ZIP64 locator signature is missing".to_owned(),
            });
        }
        let zip64 = u64::from_le_bytes(locator_bytes[8..16].try_into().unwrap());
        file.seek(SeekFrom::Start(zip64))
            .map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;
        let mut record = [0u8; 56];
        file.read_exact(&mut record)
            .map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;
        if record[..4] != [0x50, 0x4b, 0x06, 0x06] {
            return Err(PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "ZIP64 end-of-central-directory signature is missing".to_owned(),
            });
        }
        (
            u64::from_le_bytes(record[32..40].try_into().unwrap()),
            u64::from_le_bytes(record[40..48].try_into().unwrap()),
            u64::from_le_bytes(record[48..56].try_into().unwrap()),
            zip64,
        )
    } else {
        (
            entries_16 as u64,
            central_size_32 as u64,
            central_offset_32 as u64,
            eocd_absolute,
        )
    };
    if (disk != 0 && disk != u16::MAX)
        || (central_disk != 0 && central_disk != u16::MAX)
        || (entries_on_disk_16 != u16::MAX && entries_on_disk_16 as u64 != entries)
    {
        return Err(PackageError::MalformedArchive {
            archive: path.to_path_buf(),
            detail: "multi-disk ZIP archives are not supported".to_owned(),
        });
    }
    if entries > max_entries as u64 {
        return Err(PackageError::TooManyEntries {
            path: path.to_path_buf(),
            count: usize::try_from(entries).unwrap_or(usize::MAX),
            limit: max_entries,
        });
    }
    let archive_offset = central_record
        .checked_sub(central_offset.checked_add(central_size).ok_or_else(|| {
            PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "central-directory bounds overflow".to_owned(),
            }
        })?)
        .ok_or_else(|| PackageError::MalformedArchive {
            archive: path.to_path_buf(),
            detail: "central-directory offset is outside the archive".to_owned(),
        })?;
    let central_start =
        archive_offset
            .checked_add(central_offset)
            .ok_or_else(|| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "central-directory bounds overflow".to_owned(),
            })?;
    let central_end =
        central_start
            .checked_add(central_size)
            .ok_or_else(|| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "central-directory bounds overflow".to_owned(),
            })?;
    if central_end > length || central_end > eocd_absolute {
        return Err(PackageError::MalformedArchive {
            archive: path.to_path_buf(),
            detail: "central directory extends beyond the archive".to_owned(),
        });
    }
    file.seek(SeekFrom::Start(central_start))
        .map_err(|error| PackageError::MalformedArchive {
            archive: path.to_path_buf(),
            detail: error.to_string(),
        })?;
    let mut seen = BTreeSet::<Vec<u8>>::new();
    for _ in 0..entries {
        let mut header = [0u8; 46];
        file.read_exact(&mut header)
            .map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;
        if header[..4] != [0x50, 0x4b, 0x01, 0x02] {
            return Err(PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "central-directory entry signature is invalid".to_owned(),
            });
        }
        let name_length = u16::from_le_bytes([header[28], header[29]]) as usize;
        let extra_length = u64::from(u16::from_le_bytes([header[30], header[31]]));
        let comment_length = u64::from(u16::from_le_bytes([header[32], header[33]]));
        let mut name = vec![0; name_length];
        file.read_exact(&mut name)
            .map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;
        let after_name = file
            .stream_position()
            .map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;
        let after_entry = after_name
            .checked_add(extra_length + comment_length)
            .ok_or_else(|| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "central-directory entry bounds overflow".to_owned(),
            })?;
        if after_entry > central_end {
            return Err(PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: "central-directory entry extends beyond its bounds".to_owned(),
            });
        }
        if !seen.insert(name.clone()) {
            if let Ok(name) = std::str::from_utf8(&name) {
                if safe_relative_path(name).is_some() {
                    return Err(PackageError::DuplicateEntryName {
                        archive: path.to_path_buf(),
                        entry: name.to_owned(),
                    });
                }
            }
        }
        file.seek(SeekFrom::Start(after_entry))
            .map_err(|error| PackageError::MalformedArchive {
                archive: path.to_path_buf(),
                detail: error.to_string(),
            })?;
    }
    Ok(())
}

/// A file entry that survived validation and is worth extracting.
#[derive(Debug)]
struct PlannedEntry {
    /// Index in the archive's central directory.
    index: usize,
    /// Validated package-relative path.
    relative: PathBuf,
    /// Uncompressed size the archive declared, already within the caps.
    declared_size: u64,
}

/// What a package exposes, once its files are classified.
#[derive(Debug)]
struct PackageContents {
    main_module: String,
    modules: Vec<PackageModule>,
    resources: Vec<PathBuf>,
}

type ArchiveReader = BufReader<File>;

/// Why an entry copy stopped early.
#[derive(Debug)]
enum CopyFailure {
    /// The archive would not yield the entry's bytes.
    Read(io::Error),
    /// The extraction directory would not take them.
    Write(io::Error),
    /// The entry is bigger than it declared; the value is the size observed
    /// when the copy was abandoned.
    Oversized(u64),
}

/// Splits `files` into importable modules and resources, then picks the plugin
/// entry point.
///
/// `reported` is the path the caller named, so a refusal points at the archive
/// or directory the user knows about rather than at a cache directory.
fn package_contents<I>(id: &str, reported: &Path, files: I) -> Result<PackageContents, PackageError>
where
    I: IntoIterator<Item = PathBuf>,
{
    let (modules, resources) = classify(files);

    let main_module = modules
        .keys()
        .find(|name| is_top_level(name) && name.as_str() == id)
        .or_else(|| modules.keys().find(|name| is_top_level(name)))
        .cloned()
        .ok_or_else(|| PackageError::NoPluginModule {
            package: PackageId(id.to_owned()),
            root: reported.to_path_buf(),
        })?;

    Ok(PackageContents {
        main_module,
        modules: modules
            .into_iter()
            .map(|(import_name, relative_path)| PackageModule {
                import_name,
                relative_path,
            })
            .collect(),
        resources: resources.into_iter().collect(),
    })
}

/// Sorts package-relative paths into `(import name -> path)` and resources.
///
/// The ordered collections are the whole point: they deduplicate, and they fix
/// the report order at insertion time so a plugin's import set never depends on
/// `read_dir` order or on an archive's entry order.
fn classify<I>(files: I) -> (BTreeMap<String, PathBuf>, BTreeSet<PathBuf>)
where
    I: IntoIterator<Item = PathBuf>,
{
    let mut modules: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut resources: BTreeSet<PathBuf> = BTreeSet::new();

    for relative in files {
        if relative.extension().and_then(OsStr::to_str) != Some(PYTHON_EXTENSION) {
            resources.insert(relative);
            continue;
        }
        // A `.py` file is never a resource, so one without an import name —
        // a root `__init__.py`, whose dotted name would be empty because the
        // package root *is* the import root, or a path this platform cannot
        // render as UTF-8 — is reported as neither rather than mislabelled.
        let Some(import_name) = import_name(&relative) else {
            continue;
        };
        match modules.entry(import_name) {
            Entry::Vacant(slot) => {
                slot.insert(relative);
            }
            Entry::Occupied(mut slot) => {
                // `lib/__init__.py` and `lib.py` claim the same name. CPython's
                // path finder resolves the package before the module, so the
                // reported set says what an import would actually get.
                if is_package_init(&relative) && !is_package_init(slot.get()) {
                    slot.insert(relative);
                }
            }
        }
    }

    (modules, resources)
}

/// Dotted import name of a package-relative `.py` path, or `None` when the
/// path has no importable name.
fn import_name(relative: &Path) -> Option<String> {
    let stem = relative
        .file_name()?
        .to_str()?
        .strip_suffix(".py")
        .filter(|stem| !stem.is_empty())?;

    let mut name = String::new();
    for component in relative.parent().unwrap_or_else(|| Path::new("")).components() {
        let Component::Normal(part) = component else {
            return None;
        };
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(part.to_str()?);
    }

    if stem != PACKAGE_INIT_FILE.trim_end_matches(".py") {
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(stem);
    }

    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn is_package_init(relative: &Path) -> bool {
    relative.file_name() == Some(OsStr::new(PACKAGE_INIT_FILE))
}

fn is_top_level(import_name: &str) -> bool {
    !import_name.contains('.')
}

fn has_archive_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(PACKAGE_ARCHIVE_EXTENSION))
}

/// Package id for a directory or archive path.
///
/// The archive extension is stripped by length rather than with
/// [`Path::file_stem`], which would also eat a dot inside the name: `My.Tools`
/// and `My.Tools.keypirinha-package` are the same package and must produce the
/// same id.
fn package_id_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = if has_archive_extension(path) {
        // `has_archive_extension` proved the name ends with `.` plus the
        // extension, all ASCII, so this index is a char boundary.
        &name[..name.len() - (PACKAGE_ARCHIVE_EXTENSION.len() + 1)]
    } else {
        name
    };

    if id.is_empty() {
        None
    } else {
        Some(id.to_owned())
    }
}

/// Whether a ZIP entry name denotes a directory rather than a file.
fn is_directory_entry(name: &str) -> bool {
    name.ends_with('/')
}

/// Validates an archive entry name and returns the package-relative path it may
/// be written to, or `None` if it may not be written at all.
///
/// The rules are deliberately blunt. Anything that could resolve outside the
/// extraction directory on *any* supported platform is refused rather than
/// repaired, because a normalized hostile name is still a name an attacker
/// chose, and the archive that carried it has already told us what it is.
/// ZIP names use `/` as their only separator; a backslash is refused rather
/// than normalized because accepting it would make a package differ by host.
fn safe_relative_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.as_bytes().contains(&0) {
        return None;
    }
    // Rooted names are refused, never re-based: `/etc/passwd` silently
    // rewritten to `etc/passwd` would extract an attacker's file under a name
    // nobody asked about. Backslashes are not ZIP separators and are refused
    // before any platform-specific path interpretation can happen.
    if name.starts_with('/') || name.contains('\\') {
        return None;
    }

    let mut relative = PathBuf::new();
    for part in name.split('/') {
        // `a//b` and `./a` are noise, not escapes.
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        // `C:evil` is drive-relative on Windows and `file:stream` opens an
        // NTFS alternate data stream; neither is expressible as a plain
        // package-relative path.
        if part.contains(':') {
            return None;
        }
        relative.push(part);
    }

    if relative.as_os_str().is_empty() {
        None
    } else {
        Some(relative)
    }
}

/// Copies at most `allowed` bytes from `reader` to `writer`.
///
/// The bound is checked *before* each chunk is written, so a deflate bomb
/// never reaches the disk — not even the 64 KiB prefix that would be needed to
/// notice it afterwards.
fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    allowed: u64,
    buffer: &mut [u8],
) -> Result<u64, CopyFailure> {
    let mut written = 0u64;
    loop {
        let read = reader.read(buffer).map_err(CopyFailure::Read)?;
        if read == 0 {
            return Ok(written);
        }
        let next = written.saturating_add(read as u64);
        if next > allowed {
            return Err(CopyFailure::Oversized(next));
        }
        writer.write_all(&buffer[..read]).map_err(CopyFailure::Write)?;
        written = next;
    }
}

/// FNV-1a over the archive's bytes, mixed with its length.
///
/// Content addressing needs a digest that is stable across processes and Rust
/// releases, which rules out `DefaultHasher`. It does not need collision
/// resistance against an attacker: the only thing a collision could buy is
/// serving the *previously* extracted bytes of a package the attacker has
/// already replaced on disk.
fn content_digest(path: &Path) -> io::Result<u64> {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; EXTRACTION_BUFFER_BYTES];
    let mut digest = OFFSET_BASIS;
    let mut length = 0u64;

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        length = length.saturating_add(read as u64);
        for &byte in &buffer[..read] {
            digest ^= u64::from(byte);
            digest = digest.wrapping_mul(PRIME);
        }
    }

    digest ^= length;
    Ok(digest.wrapping_mul(PRIME))
}

/// Whether `name` is this loader's extraction directory for package `id`.
///
/// The digest is fixed width, which is what keeps the sweep from mistaking the
/// package `foo-bar` for a stale extraction of the package `foo`.
fn is_extraction_directory(name: &str, id: &str) -> bool {
    let Some(digest) = name.strip_prefix(id).and_then(|rest| rest.strip_prefix('-')) else {
        return false;
    };
    digest.len() == DIGEST_HEX_WIDTH && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Returns true only for an actual directory, never for a symlink to one.
///
/// The extraction path is content-addressed and may already exist, but a
/// symlink planted at that name must not turn cache reuse into a read from
/// outside the cache root.
fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
}

/// A hidden, unique name for a directory that is on its way in or out.
///
/// The leading dot keeps it out of the content-addressed namespace, so neither
/// a reuse check nor a sweep can ever confuse it for an extraction.
fn scratch_name(kind: &str) -> String {
    format!(
        ".{kind}-{pid}-{unique}",
        pid = std::process::id(),
        unique = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
    )
}

/// Renders raw entry-name bytes readably without pretending they decode.
fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_graphic() || byte == b' ' {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "\\x{byte:02X}");
        }
    }
    out
}
