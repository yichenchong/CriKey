//! Translating a Keypirinha package into a CriKey package directory
//! (spec 23.3, 28 `crikey package migrate-keypirinha`).
//!
//! # Why the Legacy Compatibility Layer owns this
//!
//! Migration reads a `.keypirinha-package` archive, resolves its entry-point
//! module, and enumerates its importable modules and resources. [`PackageLoader`]
//! already does all of that, under bounds a hostile archive cannot escape. A
//! second reader in the package manager would be a second place for a path
//! traversal to be got wrong, so this is a fold over an already loaded
//! [`LegacyPackage`] rather than a new archive reader.
//!
//! # What a migration may claim
//!
//! The `.keypirinha-package` format is a plain ZIP of Python modules. It carries
//! no version, no display name, no interpreter requirement and no dependency
//! metadata, because Keypirinha never needed any: the loader read the modules and
//! the plugin described itself in Python at runtime. A CriKey manifest has fields
//! for all four (spec 19.1), and filling them in from nothing would produce a
//! manifest that *looks* authoritative and is not — the single worst outcome for
//! a tool whose output feeds `crikey package verify` and the compatibility
//! corpus.
//!
//! So the generated manifest declares only the facts the archive genuinely
//! establishes — id, entry point, `legacy-python` runtime and `legacy-strict`
//! scheduling (spec 7.2) — and everything else is reported as a
//! [`MigrationLimitation`] for a human to fill in. The version field is required
//! by the manifest schema and cannot be omitted, so it carries a sentinel that no
//! release could plausibly be rather than a plausible-looking `0.1.0`.
//!
//! # What this is not
//!
//! Not a compatibility report. Whether the package imports `keypirinha_wintypes`,
//! calls an unimplemented API, or blocks in a callback without polling
//! `should_terminate()` are §26.2 findings, and [`crate::LegacyDiagnostics`] is
//! where they live. Duplicating even one of them here would give an operator two
//! places to look and two chances to read a stale answer.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crikey_plugin_model::{Manifest, ManifestError};

use crate::package::{LegacyPackage, PackageError, PackageLimits, PackageLoader};

/// The version a migrated manifest carries.
///
/// Deliberately not a plausible release number: the source format declares no
/// version, and a manifest that said `0.1.0` would be indistinguishable from one
/// whose author chose it. `+` opens semver build metadata, so this remains a
/// well-formed version string that no comparison ever ranks as a real release.
pub const MIGRATED_VERSION: &str = "0.0.0+keypirinha-migrated";

/// Whether `character` may appear in a migrated plugin id.
///
/// A Keypirinha id is a file-system name and may hold anything the filesystem
/// allows, while a CriKey plugin id ends up in a TOML key, a namespaced plugin
/// id (`legacy.<id>`), an environment variable and a directory name. Restricting
/// it here — and reporting the substitution — keeps every one of those valid
/// without silently deciding that the migrated plugin is a different plugin.
fn is_allowed_id_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_')
}

/// A fact the source format does not carry, so the generated manifest does not
/// claim it.
///
/// Each variant is a typed value rather than a rendered sentence, so the CLI can
/// print a stable machine code beside the prose and a script can act on the code
/// without parsing English — the same split [`crate::CompatibilityWarning`] makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationLimitation {
    /// The archive declares no version, so [`MIGRATED_VERSION`] was written.
    NoDeclaredVersion,
    /// The archive declares no display name, so the id was reused as the name.
    NoDeclaredName,
    /// The archive declares no interpreter requirement, so no `[python]`
    /// `requires-python` was written.
    NoDeclaredPythonRequirement,
    /// The archive declares no dependencies, so no `[python] dependencies` was
    /// written. Anything the plugin needs is vendored inside it and was copied.
    NoDeclaredDependencies,
    /// The package id had to change to be a valid CriKey plugin id.
    IdSanitized {
        /// The id the source package had.
        original: String,
    },
    /// A Keypirinha settings file. Its keys and defaults live in the plugin's
    /// Python, not in the file, so no `[configuration]` fields can be derived.
    SettingsFile {
        /// Package-relative path of the settings file.
        path: PathBuf,
    },
    /// A compiled extension module. It is copied, but a CriKey native plugin
    /// declares a per-target entry point (spec 19.3) and nothing in the archive
    /// says which targets this binary was built for.
    NativeExtension {
        /// Package-relative path of the extension.
        path: PathBuf,
    },
    /// A `.py` file the legacy loader does not treat as an importable module:
    /// a root `__init__.py`, a name no import statement can spell, or the
    /// losing side of a `lib.py` / `lib/__init__.py` collision. It is copied,
    /// because the source package has it on disk and may open it as
    /// package-relative data, but nothing imports it under that name.
    UnimportableModule {
        /// Package-relative path of the file.
        path: PathBuf,
    },
}

impl MigrationLimitation {
    /// Stable machine handle. Never reworded once shipped; the prose in
    /// [`Self::message`] may be.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoDeclaredVersion => "no-declared-version",
            Self::NoDeclaredName => "no-declared-name",
            Self::NoDeclaredPythonRequirement => "no-declared-python-requirement",
            Self::NoDeclaredDependencies => "no-declared-dependencies",
            Self::IdSanitized { .. } => "id-sanitized",
            Self::SettingsFile { .. } => "settings-file",
            Self::NativeExtension { .. } => "native-extension",
            Self::UnimportableModule { .. } => "unimportable-module",
        }
    }

    /// What was not translated, and what a human has to do about it.
    pub fn message(&self) -> String {
        match self {
            Self::NoDeclaredVersion => format!(
                "the Keypirinha package format carries no version; `version` was written as \
                 `{MIGRATED_VERSION}` and must be replaced with the real one before publishing"
            ),
            Self::NoDeclaredName => {
                "the Keypirinha package format carries no display name; `name` reuses the plugin id \
                 and should be replaced with the name users should see"
                    .to_owned()
            }
            Self::NoDeclaredPythonRequirement => {
                "the Keypirinha package format declares no interpreter requirement; no \
                 `[python] requires-python` was written, so the Legacy Compatibility Layer's \
                 supported floor applies"
                    .to_owned()
            }
            Self::NoDeclaredDependencies => {
                "the Keypirinha package format declares no dependencies; no `[python] dependencies` \
                 was written and every vendored module was copied as-is"
                    .to_owned()
            }
            Self::IdSanitized { original } => format!(
                "the source package id `{original}` is not a valid CriKey plugin id; confirm the \
                 substituted id is the one you want, because it is what settings and history key on"
            ),
            Self::SettingsFile { path } => format!(
                "`{}` is a Keypirinha settings file; its keys and defaults are defined in the \
                 plugin's Python rather than in the file, so no `[configuration]` fields could be \
                 derived and the plugin keeps reading it through the compatibility layer",
                path.display()
            ),
            Self::NativeExtension { path } => format!(
                "`{}` is a compiled extension module; it was copied, but nothing in the archive \
                 says which platform and architecture it was built for, so no `[platform]` lists \
                 were written",
                path.display()
            ),
            Self::UnimportableModule { path } => format!(
                "`{}` is a Python file the loader does not expose as an importable module — a root \
                 `__init__.py`, a name no import statement can spell, or the losing side of a \
                 `lib.py` / `lib/__init__.py` collision. It was copied verbatim so the package \
                 keeps whatever it reads as package-relative data, but no `import` resolves to it",
                path.display()
            ),
        }
    }
}

/// What one migration produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Plugin id written into the manifest, after sanitizing.
    pub id: String,
    /// The package directory that was written.
    pub destination: PathBuf,
    /// Dotted import name of the plugin entry point.
    pub entrypoint: String,
    /// Import names of every module copied, sorted.
    pub modules: Vec<String>,
    /// Package-relative paths of every non-module file copied, sorted.
    pub resources: Vec<PathBuf>,
    /// Everything the source format does not carry, in report order.
    pub limitations: Vec<MigrationLimitation>,
}

/// Why a migration did not happen.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The source could not be read as a legacy package.
    #[error("cannot read the Keypirinha package: {0}")]
    Source(#[from] PackageError),

    /// The destination already holds something. Never overwritten: a migration
    /// that silently replaced a directory would destroy hand-edits made to the
    /// manifest it wrote on the previous run, which is the whole workflow.
    #[error("`{}` already exists; migrate into a new directory", path.display())]
    DestinationExists {
        /// The destination as the caller named it.
        path: PathBuf,
    },

    /// The package id holds no character a CriKey plugin id may contain, so
    /// there is nothing to substitute.
    #[error("package id `{original}` has no character a CriKey plugin id may contain")]
    UnusableId {
        /// The id that was refused.
        original: String,
    },

    /// The manifest this module generated does not parse. A bug here, never the
    /// caller's fault: it is checked before anything is written, so a migration
    /// can never leave an invalid manifest on disk.
    #[error("the generated manifest is invalid: {0}")]
    GeneratedManifest(#[from] ManifestError),

    /// The filesystem refused a write the migration needs.
    #[error("`{}` could not be written: {error}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        error: io::Error,
    },

    /// One file in the source package is larger than a single package file may
    /// be. Refused before anything is written, exactly as the archive reader
    /// refuses an oversized entry: a loose directory is no more trustworthy
    /// than a ZIP, and a sparse multi-gigabyte file in one would otherwise be
    /// read whole into the CLI process.
    #[error("`{}` is {size} bytes, over the {limit} byte limit for one package file", path.display())]
    FileTooLarge {
        /// Package-relative path of the offending file.
        path: PathBuf,
        /// Its size on disk.
        size: u64,
        /// [`PackageLimits::max_entry_bytes`].
        limit: u64,
    },

    /// The source package's files total more than a whole package may.
    #[error("the package holds at least {size} bytes, over the {limit} byte package limit")]
    PackageTooLarge {
        /// Bytes counted when the ceiling was passed.
        size: u64,
        /// [`PackageLimits::max_total_bytes`].
        limit: u64,
    },
}

/// Converts the Keypirinha package at `source` into a CriKey package directory
/// at `destination`, extracting archives under `cache_root`.
///
/// `destination` must not exist. `cache_root` is the caller's trust root for
/// archive extraction and must be a private per-user directory, exactly as
/// [`PackageLoader`] requires everywhere else: the loader trusts an already
/// extracted content-addressed directory, so a shared temporary root would let
/// another local process decide what gets copied into the migrated package.
pub fn migrate_keypirinha_package(
    source: &Path,
    destination: &Path,
    cache_root: &Path,
) -> Result<MigrationReport, MigrationError> {
    if destination.exists() {
        return Err(MigrationError::DestinationExists {
            path: destination.to_path_buf(),
        });
    }

    let loader = PackageLoader::new(cache_root.to_path_buf());
    let package = loader.load(source)?;
    // Planned before anything is written and under the loader's own ceilings, so
    // an oversized loose file is refused with no destination on disk at all.
    let plan = CopyPlan::of(&package, loader.limits())?;
    let mut limitations = Vec::new();

    let original = package.id.as_str().to_owned();
    let id = sanitize_id(&original)?;
    if id != original {
        limitations.push(MigrationLimitation::IdSanitized { original });
    }

    // Report order is the order a human works through it: the fields they must
    // edit, then the ones they may, then the files they have to decide about.
    limitations.push(MigrationLimitation::NoDeclaredVersion);
    limitations.push(MigrationLimitation::NoDeclaredName);
    limitations.push(MigrationLimitation::NoDeclaredPythonRequirement);
    limitations.push(MigrationLimitation::NoDeclaredDependencies);
    for resource in &package.resources {
        if is_settings_file(resource) {
            limitations.push(MigrationLimitation::SettingsFile {
                path: resource.clone(),
            });
        } else if is_native_extension(resource) {
            limitations.push(MigrationLimitation::NativeExtension {
                path: resource.clone(),
            });
        }
    }
    for path in &plan.unimportable {
        limitations.push(MigrationLimitation::UnimportableModule { path: path.clone() });
    }

    let manifest = render_manifest(&id, &package.main_module);
    // Checked before a single byte is written. A generated manifest that does not
    // parse is a defect in this function, and the operator must not have to
    // discover it by running `crikey run` against a half-migrated directory.
    Manifest::parse(&manifest)?;

    plan.copy(package.root.content_root(), destination, loader.limits())?;
    write_file(&destination.join("crikey.toml"), manifest.as_bytes())?;

    Ok(MigrationReport {
        id,
        destination: destination.to_path_buf(),
        entrypoint: package.main_module.clone(),
        modules: package
            .modules
            .iter()
            .map(|module| module.import_name.clone())
            .collect(),
        resources: package.resources.clone(),
        limitations,
    })
}

/// The id with every character a CriKey plugin id may not hold replaced by `-`.
///
/// Substitution rather than refusal, because a Keypirinha package named
/// `My Plugin` is a real package an operator wants migrated; refusal is reserved
/// for an id that would become nothing but separators, where no substitution
/// preserves any of the author's intent.
fn sanitize_id(original: &str) -> Result<String, MigrationError> {
    let sanitized: String = original
        .chars()
        .map(|character| {
            if is_allowed_id_char(character) {
                character
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.chars().all(|character| character == '-') {
        return Err(MigrationError::UnusableId {
            original: original.to_owned(),
        });
    }
    Ok(sanitized)
}

/// Whether `path` is a Keypirinha settings file (spec 14.7).
fn is_settings_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ini"))
}

/// Whether `path` is a compiled Python extension for some platform.
fn is_native_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["pyd", "dll", "so", "dylib"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// The generated `crikey.toml`, claiming only what the archive establishes.
///
/// Written as text rather than serialized from a [`Manifest`] value for two
/// reasons: a serialized manifest emits every defaulted section, which reads as
/// a set of deliberate declarations the migration never made; and the comments
/// are the part an operator actually needs, because they say which two fields
/// are placeholders. The text is parsed back before use, so it cannot drift from
/// what the schema accepts.
fn render_manifest(id: &str, entrypoint: &str) -> String {
    format!(
        "# Generated by `crikey package migrate-keypirinha`.\n\
         #\n\
         # The Keypirinha package format carries no version, display name,\n\
         # interpreter requirement or dependency list, so this manifest declares\n\
         # none of them. `version` below is a placeholder and `name` reuses the\n\
         # plugin id; replace both before publishing.\n\
         manifest-version = 1\n\
         \n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"{id}\"\n\
         version = \"{MIGRATED_VERSION}\"\n\
         runtime = \"legacy-python\"\n\
         scheduling-profile = \"legacy-strict\"\n\
         entrypoint = \"{entrypoint}\"\n"
    )
}

/// Copies every module and resource of `package` into `destination`, preserving
/// package-relative paths so the entry-point import still resolves.
/// A complete, bounded plan of the source tree. The loader's classification is
/// intentionally lossy for `.py` files; migration must not be.
struct CopyPlan {
    files: Vec<PathBuf>,
    unimportable: Vec<PathBuf>,
}

impl CopyPlan {
    fn of(package: &LegacyPackage, limits: PackageLimits) -> Result<Self, MigrationError> {
        let mut files = Vec::new();
        let mut pending = vec![PathBuf::new()];
        let root = package.root.content_root();
        let known: BTreeSet<PathBuf> = package
            .modules
            .iter()
            .map(|module| module.relative_path.clone())
            .chain(package.resources.iter().cloned())
            .collect();
        while let Some(relative_dir) = pending.pop() {
            let absolute = root.join(&relative_dir);
            let entries = fs::read_dir(&absolute).map_err(|error| MigrationError::Io {
                path: absolute.clone(),
                error,
            })?;
            for entry in entries {
                let entry = entry.map_err(|error| MigrationError::Io {
                    path: absolute.clone(),
                    error,
                })?;
                let ty = entry.file_type().map_err(|error| MigrationError::Io {
                    path: entry.path(),
                    error,
                })?;
                let relative = relative_dir.join(entry.file_name());
                if ty.is_dir() {
                    pending.push(relative);
                } else if ty.is_file() {
                    files.push(relative);
                }
            }
        }
        files.sort();
        let mut total = 0u64;
        for relative in &files {
            let path = root.join(relative);
            let size = fs::metadata(&path)
                .map_err(|error| MigrationError::Io {
                    path: path.clone(),
                    error,
                })?
                .len();
            if size > limits.max_entry_bytes {
                return Err(MigrationError::FileTooLarge {
                    path: relative.clone(),
                    size,
                    limit: limits.max_entry_bytes,
                });
            }
            total = total.saturating_add(size);
            if total > limits.max_total_bytes {
                return Err(MigrationError::PackageTooLarge {
                    size: total,
                    limit: limits.max_total_bytes,
                });
            }
        }
        let unimportable = files
            .iter()
            .filter(|path| {
                path.extension().and_then(|extension| extension.to_str()) == Some("py")
                    && !known.contains(*path)
            })
            .cloned()
            .collect();
        Ok(Self { files, unimportable })
    }

    fn copy(
        &self,
        content_root: &Path,
        destination: &Path,
        limits: PackageLimits,
    ) -> Result<(), MigrationError> {
        for relative in &self.files {
            let source = content_root.join(relative);
            let mut input = File::open(&source).map_err(|error| MigrationError::Io {
                path: source.clone(),
                error,
            })?;
            let output = destination.join(relative);
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent).map_err(|error| MigrationError::Io {
                    path: parent.to_path_buf(),
                    error,
                })?;
            }
            let mut writer = File::create(&output).map_err(|error| MigrationError::Io {
                path: output.clone(),
                error,
            })?;
            let mut limited = (&mut input).take(limits.max_entry_bytes.saturating_add(1));
            let copied = io::copy(&mut limited, &mut writer).map_err(|error| MigrationError::Io {
                path: source.clone(),
                error,
            })?;
            if copied > limits.max_entry_bytes {
                return Err(MigrationError::FileTooLarge {
                    path: relative.clone(),
                    size: copied,
                    limit: limits.max_entry_bytes,
                });
            }
        }
        Ok(())
    }
}

/// Writes `bytes` to `path`, creating parent directories.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), MigrationError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| MigrationError::Io {
            path: parent.to_path_buf(),
            error,
        })?;
    }
    fs::write(path, bytes).map_err(|error| MigrationError::Io {
        path: path.to_path_buf(),
        error,
    })
}
