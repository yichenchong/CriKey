//! Native plugin package archives and atomic installation (spec 23.3, 23.4).
//!
//! The package format is deliberately small: a ZIP archive contains the native
//! manifest and payload, plus a TOML lock member that authenticates every other
//! member.  Installation validates the complete archive before it changes the
//! installed directory, then swaps complete directories rather than replacing
//! individual files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crikey_plugin_model::{Manifest, Runtime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::index::constant_time_hex_eq;
use crate::signature::{PackageSigningKey, SignaturePolicy, SignatureState};
use crate::PackageError;

const MANIFEST_MEMBER: &str = "crikey.toml";
const LOCK_MEMBER: &str = "crikey-package.lock";

/// Ceiling on a package file this crate reads whole into memory.
pub(crate) const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = MAX_PACKAGE_BYTES;
const MAX_MEMBERS: usize = 65_536;
const SWAP_JOURNAL_SUFFIX: &str = ".crikey-swap";

/// The first line of the byte string a package signature covers.
///
/// Versioned and domain-separated: a signature over a CriKey package can never
/// be replayed as a signature over anything else CriKey signs, and a future
/// change to the manifest layout gets a new preamble rather than silently
/// meaning something different to an older verifier.
const CANONICAL_MANIFEST_PREAMBLE: &str = "crikey-package-signature-v1";

/// A package id is also used as an install-directory name.
pub(crate) fn safe_id_component(id: &str) -> Result<&str, PackageError> {
    let invalid = || PackageError::Manifest(format!("plugin id {id:?} is not a safe path component"));
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.starts_with('.')
        || id.contains(['/', '\\', ':'])
        || id.as_bytes().contains(&0)
    {
        return Err(invalid());
    }
    let mut components = Path::new(id).components();
    if !matches!(components.next(), Some(Component::Normal(part)) if part.to_str() == Some(id))
        || components.next().is_some()
        || is_windows_device_name(id)
    {
        return Err(invalid());
    }
    Ok(id)
}

fn is_windows_device_name(id: &str) -> bool {
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = id.split('.').next().unwrap_or(id);
    DEVICES.iter().any(|device| stem.eq_ignore_ascii_case(device))
}

/// Metadata reported for a validated native package (spec 23.3, 23.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePackageReport {
    pub plugin: String,
    pub version: String,
    pub os: Vec<String>,
    pub arch: Vec<String>,
    pub entries: Vec<(String, u64)>,
    pub hash: String,
    /// Whether provenance was established, and by whom (spec 2.2; ADR 0012).
    ///
    /// [`SignatureState::Unchecked`] is not a synonym for "unsigned": it means
    /// this report came from an entry point that was given no
    /// [`SignaturePolicy`] and therefore never looked. Reporting `unsigned`
    /// there would be a claim about the package that nothing had established.
    pub signature: SignatureState,
    pub unsigned_binary: bool,
}

/// A completed native installation and the retained directory used for
/// rollback (spec 23.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeInstall {
    pub root: PathBuf,
    pub previous: Option<PathBuf>,
    pub report: NativePackageReport,
}

/// What `crikey package sign` produced (spec 2.2; ADR 0012).
///
/// Carries the fingerprint rather than the public key: the fingerprint is what
/// an operator compares against the one a publisher advertises, and it is what
/// every message about this package will quote from now on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSignatureReport {
    pub plugin: String,
    pub version: String,
    /// SHA-256 of the archive that was signed.
    pub hash: String,
    /// Fingerprint of the key that signed it.
    pub fingerprint: String,
    /// Where the detached signature was written.
    pub signature: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageLock {
    plugin: String,
    version: String,
    entries: BTreeMap<String, String>,
}

#[derive(Debug)]
struct SourceMember {
    bytes: Vec<u8>,
    unix_mode: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct ArchiveMember {
    pub(crate) bytes: Vec<u8>,
    pub(crate) directory: bool,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) unix_mode: Option<u32>,
}

#[derive(Debug)]
struct LoadedPackage {
    members: BTreeMap<String, ArchiveMember>,
    manifest: Manifest,
    archive_hash: String,
}

/// Builds a deterministic native ZIP package and returns its validated report.
///
/// Archive paths are sorted, timestamps use the ZIP crate's fixed default (the
/// crate is built without its wall-clock `time` feature), and all payloads are
/// stored.  This satisfies the byte-stable package requirement in amendment
/// §11.3 and spec §23.3.
pub fn build_package(plugin_dir: &Path, out: &Path) -> Result<NativePackageReport, PackageError> {
    let manifest_path = plugin_dir.join(MANIFEST_MEMBER);
    let manifest_bytes = fs::read(&manifest_path)?;
    let manifest = parse_manifest(&manifest_bytes)?;
    validate_manifest_shape(&manifest)?;

    let source_members = collect_source_members(plugin_dir, out)?;
    if !source_members.contains_key(MANIFEST_MEMBER) {
        return Err(PackageError::Manifest(format!(
            "package is missing {MANIFEST_MEMBER}"
        )));
    }
    if source_members.contains_key(LOCK_MEMBER) {
        return Err(PackageError::Manifest(format!(
            "{LOCK_MEMBER} is generated by the package builder"
        )));
    }
    validate_manifest_members(
        &manifest,
        source_members.keys().cloned(),
        std::iter::empty::<String>(),
    )?;

    let entries = source_members
        .iter()
        .map(|(name, member)| (name.clone(), sha256_hex(&member.bytes)))
        .collect::<BTreeMap<_, _>>();
    let lock = PackageLock {
        plugin: manifest.plugin.id.clone(),
        version: manifest.plugin.version.clone(),
        entries,
    };
    let lock_bytes = toml::to_string(&lock)
        .map_err(|error| PackageError::Manifest(format!("could not encode package lock: {error}")))?
        .into_bytes();

    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(out)?;
    let mut writer = ZipWriter::new(file);
    for (name, member) in &source_members {
        let options = archive_options(member.unix_mode);
        writer
            .start_file(name.as_str(), options)
            .map_err(|error| PackageError::MalformedArchive(format!("could not create {name}: {error}")))?;
        writer.write_all(&member.bytes)?;
    }
    writer
        .start_file(LOCK_MEMBER, archive_options(None))
        .map_err(|error| {
            PackageError::MalformedArchive(format!("could not create {LOCK_MEMBER}: {error}"))
        })?;
    writer.write_all(&lock_bytes)?;
    writer
        .finish()
        .map_err(|error| PackageError::MalformedArchive(format!("could not finish package: {error}")))?;

    inspect_package(out)
}

/// Inspects and authenticates every member of a native package archive.
///
/// Inspection is not verification of provenance: the returned report says
/// [`SignatureState::Unchecked`], because no trust store was supplied and
/// therefore nothing was decided. Use [`verify_package_with_policy`] to ask
/// who signed a package.
pub fn inspect_package(archive: &Path) -> Result<NativePackageReport, PackageError> {
    let package = load_package(archive)?;
    validate_integrity(&package)?;
    Ok(package_report(&package, SignatureState::Unchecked))
}

/// Authenticates a native package and, when supplied, pins its whole-archive
/// SHA-256 to `expected_hash` (spec 23.3).
///
/// "Authenticates" means the same thing here as it does for
/// [`inspect_package`] and [`install_native`]: every member is checked against
/// the digest the embedded lock claims for it. An earlier version checked only
/// the archive's shape and the optional whole-archive hash, which meant
/// `crikey package verify` accepted an archive whose payload had been swapped
/// and whose lock no longer described it — the one thing the command exists to
/// refuse. The optional `expected_hash` is an *additional* out-of-band pin, not
/// a substitute for the embedded digests.
///
/// What none of that establishes is *provenance*. A lock authenticates the
/// archive against itself, so a hostile party who rebuilds the archive and
/// rewrites the lock to match produces a package that passes every check here.
/// This entry point therefore reports [`SignatureState::Unchecked`];
/// [`verify_package_with_policy`] is the one that answers "signed by whom".
pub fn verify_package(
    archive: &Path,
    expected_hash: Option<&str>,
) -> Result<NativePackageReport, PackageError> {
    verify_package_with_policy(archive, expected_hash, &SignaturePolicy::unchecked())
}

/// Authenticates a native package and establishes its provenance under
/// `policy` (spec 2.2, 23.3; ADR 0012).
///
/// Order matters and is deliberate: the archive is validated against its own
/// embedded lock *first*, then the signature is checked over the canonical
/// manifest of those validated members. Checking the signature first would mean
/// verifying a signature over digests that had not yet been shown to describe
/// the bytes on disk.
pub fn verify_package_with_policy(
    archive: &Path,
    expected_hash: Option<&str>,
    policy: &SignaturePolicy,
) -> Result<NativePackageReport, PackageError> {
    let package = load_package(archive)?;
    validate_integrity(&package)?;
    if let Some(expected) = expected_hash {
        if !is_hex_sha256(expected) || !constant_time_hex_eq(&package.archive_hash, expected) {
            return Err(PackageError::HashMismatch(format!(
                "archive hash is {}, expected {expected}",
                package.archive_hash
            )));
        }
    }
    let signature = evaluate_package_signature(archive, &package, policy)?;
    Ok(package_report(&package, signature))
}

/// Signs a package: what `crikey package sign` does once the archive is built.
///
/// The package is authenticated against its embedded lock before anything is
/// signed. Signing an archive without checking it first would let a plugin
/// author put their name on bytes they had not looked at, which is the one
/// thing a signature is supposed to rule out.
pub fn sign_package(
    archive: &Path,
    key: &PackageSigningKey,
    signature_out: &Path,
) -> Result<PackageSignatureReport, PackageError> {
    let package = load_package(archive)?;
    validate_integrity(&package)?;
    let payload = canonical_manifest(&package);
    let manifest = key.detached(&payload);
    crate::signature::write_signature_file(signature_out, &manifest)?;
    Ok(PackageSignatureReport {
        plugin: package.manifest.plugin.id.clone(),
        version: package.manifest.plugin.version.clone(),
        hash: package.archive_hash,
        fingerprint: manifest.key.fingerprint(),
        signature: signature_out.to_path_buf(),
    })
}

/// Applies `policy` to the detached signature beside `archive`.
fn evaluate_package_signature(
    archive: &Path,
    package: &LoadedPackage,
    policy: &SignaturePolicy,
) -> Result<SignatureState, PackageError> {
    if matches!(policy, SignaturePolicy::Unchecked) {
        return Ok(SignatureState::Unchecked);
    }
    let signature_path = crate::signature::signature_path_for(archive);
    let payload = canonical_manifest(package);
    let artefact = archive.display().to_string();
    Ok(crate::signature::evaluate(
        &artefact,
        &signature_path,
        &payload,
        policy,
    )?)
}

/// The exact bytes a package signature covers.
///
/// Every member's name and digest, not one file: a signature over the payload
/// alone would leave the manifest and the lock free to be rewritten, and a
/// signature over the raw archive bytes would break on any re-zip that changed
/// nothing that matters.
///
/// Every field is length-prefixed, so no member name — whatever bytes a
/// publisher chose for it — can be spliced to make one package's manifest read
/// as another's. Members come from a [`BTreeMap`], so the order is the archive's
/// sorted member order and two builds of the same package produce identical
/// bytes here.
fn canonical_manifest(package: &LoadedPackage) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(CANONICAL_MANIFEST_PREAMBLE.as_bytes());
    out.push(b'\n');
    let mut field = |value: &str| {
        out.extend_from_slice(value.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    };
    field(&package.manifest.plugin.id);
    field(&package.manifest.plugin.version);
    field(&package.members.len().to_string());
    for (name, member) in &package.members {
        field(name);
        field(&sha256_hex(&member.bytes));
    }
    out
}

/// Installs a validated native package by swapping complete directories.  The
/// platform check occurs before embedded-hash verification, and the supplied
/// stop callback is invoked before any existing installation is moved (spec
/// 23.3, 23.4).
pub fn install_native(
    archive: &Path,
    install_root: &Path,
    os: &str,
    arch: &str,
    stop_running: &mut dyn FnMut(&str) -> Result<(), PackageError>,
) -> Result<NativeInstall, PackageError> {
    install_native_with_retention(archive, install_root, os, arch, stop_running, None)
}

/// Installs while retaining the displaced version at its final rollback path.
///
/// Establishes no provenance: see [`install_native_with_policy`], which this
/// delegates to with [`SignaturePolicy::unchecked`].
pub fn install_native_with_retention(
    archive: &Path,
    install_root: &Path,
    os: &str,
    arch: &str,
    stop_running: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    retention: Option<&Path>,
) -> Result<NativeInstall, PackageError> {
    install_native_with_policy(
        archive,
        install_root,
        os,
        arch,
        stop_running,
        retention,
        &SignaturePolicy::unchecked(),
    )
}

/// Installs under a provenance `policy` (spec 2.2, 23.3; ADR 0012).
///
/// The signature decision is made before `stop_running` is called and therefore
/// before anything at all moves: a package that will be refused must not have
/// cost the operator a running plugin first.
pub fn install_native_with_policy(
    archive: &Path,
    install_root: &Path,
    os: &str,
    arch: &str,
    stop_running: &mut dyn FnMut(&str) -> Result<(), PackageError>,
    retention: Option<&Path>,
    policy: &SignaturePolicy,
) -> Result<NativeInstall, PackageError> {
    let package = load_package(archive)?;
    ensure_compatible(&package.manifest, os, arch)?;
    validate_integrity(&package)?;
    let signature = evaluate_package_signature(archive, &package, policy)?;

    stop_running(&package.manifest.plugin.id)?;

    let parent = install_parent(install_root)?;
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)?;
    }

    let staging = temporary_path(parent, install_root, "staging");
    if let Err(error) = write_members(&package.members, &staging) {
        remove_directory_if_present(&staging);
        return Err(error);
    }

    // A native install root is a directory or nothing. Refusing a file or a
    // symlink here, before anything moves, keeps the swap from quietly
    // replacing something that was never an installation.
    if let Err(error) = inspect_install_root(install_root) {
        remove_directory_if_present(&staging);
        return Err(error);
    }

    let backup = retention
        .map(Path::to_path_buf)
        .unwrap_or_else(|| temporary_path(parent, install_root, "previous"));
    let previous = match swap_into_place(install_root, &staging, Some(&backup)) {
        Ok(true) => Some(backup),
        Ok(false) => None,
        Err(error) => {
            remove_directory_if_present(&staging);
            return Err(error);
        }
    };

    Ok(NativeInstall {
        root: install_root.to_path_buf(),
        previous,
        report: package_report(&package, signature),
    })
}

/// Replaces `target` with `replacement` in one rename, retaining whatever
/// `target` held at `previous` (spec 23.4).
///
/// This is the single place the crate performs an installation swap. The
/// replacement is materialised somewhere else and only then becomes the
/// target, so no half-written tree is ever reachable under `target` and a
/// failure at any step leaves the previous working version in place.
///
/// Returns whether anything was displaced. An absent or empty target displaces
/// nothing: a first install has no previous version, and retaining an empty
/// directory would let a later rollback "restore" an installation that never
/// existed.
///
/// `previous` of `None` discards the displaced target rather than retaining
/// it, which is what a rollback wants — the version being undone is not worth
/// keeping, and keeping it would make the next rollback undo the undo.
pub(crate) fn swap_into_place(
    target: &Path,
    replacement: &Path,
    previous: Option<&Path>,
) -> Result<bool, PackageError> {
    let parent = install_parent(target)?;
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)?;
    }
    let hold = match previous {
        Some(path) => {
            if let Some(holder) = path.parent().filter(|holder| !holder.as_os_str().is_empty()) {
                fs::create_dir_all(holder)?;
            }
            // One retained previous version per plugin: an older retention is
            // already superseded by the version about to be displaced.
            remove_path(path);
            path.to_path_buf()
        }
        None => temporary_path(parent, target, "displaced"),
    };

    let displaced = false;
    let mut removed_empty_target = false;
    match inspect_target(target)? {
        TargetState::Missing => {}
        TargetState::EmptyDirectory => {
            fs::remove_dir(target)?;
            removed_empty_target = true;
        }
        TargetState::Present => {
            let journal = parent.join(format!(
                ".{}{SWAP_JOURNAL_SUFFIX}",
                target.file_name().and_then(|n| n.to_str()).unwrap_or("plugin")
            ));
            write_swap_journal(&journal, target, &hold)?;
            if let Err(error) = fs::rename(target, &hold) {
                let _ = fs::remove_file(&journal);
                return Err(PackageError::Io(error));
            }
            if let Err(error) = fs::rename(replacement, target) {
                if let Err(restore) = fs::rename(&hold, target) {
                    return Err(PackageError::Install(format!(
                        "replacement failed ({error}) and restoring the previous version failed ({restore})"
                    )));
                }
                let _ = fs::remove_file(&journal);
                return Err(PackageError::Io(error));
            }
            let _ = fs::remove_file(&journal);
            if previous.is_none() {
                remove_path(&hold);
            }
            return Ok(true);
        }
    }

    if let Err(error) = fs::rename(replacement, target) {
        if displaced {
            if let Err(restore) = fs::rename(&hold, target) {
                return Err(PackageError::Install(format!(
                    "replacement failed ({error}) and restoring the previous version failed ({restore})"
                )));
            }
        } else if removed_empty_target {
            let _ = fs::create_dir(target);
        }
        return Err(PackageError::Io(error));
    }
    if displaced && previous.is_none() {
        remove_path(&hold);
    }
    Ok(displaced)
}

fn write_swap_journal(path: &Path, target: &Path, hold: &Path) -> Result<(), PackageError> {
    let target = target
        .to_str()
        .ok_or_else(|| PackageError::Install("swap path is not UTF-8".to_owned()))?;
    let hold = hold
        .to_str()
        .ok_or_else(|| PackageError::Install("swap path is not UTF-8".to_owned()))?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    writeln!(file, "{target}")?;
    writeln!(file, "{hold}")?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn recover_interrupted_swaps(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        let journal = entry.path();
        let Ok(journal_meta) = fs::symlink_metadata(&journal) else {
            continue;
        };
        if !journal_meta.file_type().is_file() {
            continue;
        }
        let Some(name) = journal.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(SWAP_JOURNAL_SUFFIX) {
            continue;
        }
        let target_name = name
            .strip_prefix('.')
            .and_then(|value| value.strip_suffix(SWAP_JOURNAL_SUFFIX))
            .filter(|value| !value.is_empty());
        let Some(target_name) = target_name else { continue };
        let Ok(text) = fs::read_to_string(&journal) else {
            continue;
        };
        let mut lines = text.lines();
        let (Some(target_text), Some(hold_text)) = (lines.next(), lines.next()) else {
            continue;
        };
        let target = Path::new(target_text);
        let hold = Path::new(hold_text);
        let expected_target = parent.join(target_name);
        let direct_child = |path: &Path| path.parent() == Some(parent);
        let retained_child = |path: &Path| {
            path.file_name().is_some_and(|name| name == target_name)
                && path.parent().and_then(Path::file_name) == parent.file_name()
                && path
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == ".previous")
                && path.parent().and_then(Path::parent).and_then(Path::parent) == parent.parent()
        };
        if target != expected_target
            || !direct_child(target)
            || !(direct_child(hold) || retained_child(hold))
            || fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink())
            || fs::symlink_metadata(hold).is_ok_and(|m| m.file_type().is_symlink())
        {
            continue;
        }
        if fs::symlink_metadata(target).is_err() && fs::symlink_metadata(hold).is_ok() {
            let _ = fs::rename(hold, target);
        }
        if fs::symlink_metadata(target).is_ok() {
            let _ = fs::remove_file(journal);
        }
    }
}

/// What a swap target currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetState {
    Missing,
    /// A directory with no entries, which is indistinguishable from "not
    /// installed" and is therefore not worth retaining.
    EmptyDirectory,
    Present,
}

fn inspect_target(target: &Path) -> Result<TargetState, PackageError> {
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(TargetState::Missing),
        Err(error) => return Err(PackageError::Io(error)),
    };
    // A symlink would make the rename replace the link rather than what the
    // user believes is installed, so it is refused rather than followed.
    if metadata.file_type().is_symlink() {
        return Err(PackageError::Install(format!(
            "{} is a symbolic link, not an installation",
            target.display()
        )));
    }
    if metadata.is_dir() && fs::read_dir(target)?.next().transpose()?.is_none() {
        return Ok(TargetState::EmptyDirectory);
    }
    Ok(TargetState::Present)
}

/// Removes a file or a whole directory, whichever is there, ignoring absence.
pub(crate) fn remove_path(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

/// Restores the retained previous directory with the same complete-directory
/// swap used by installation (spec 23.4).
pub fn rollback_native(install: &NativeInstall) -> Result<(), PackageError> {
    let previous = install
        .previous
        .as_ref()
        .ok_or_else(|| PackageError::Install("no previous native version is retained".to_owned()))?;
    if previous == &install.root {
        return Err(PackageError::Install(
            "previous native version must be separate from the active root".to_owned(),
        ));
    }
    let parent = install_parent(&install.root)?;
    if previous.parent() != Some(parent) {
        return Err(PackageError::Install(
            "previous native version is not beside the active root".to_owned(),
        ));
    }
    let previous_metadata = fs::symlink_metadata(previous).map_err(PackageError::Io)?;
    if !previous_metadata.is_dir() || previous_metadata.file_type().is_symlink() {
        return Err(PackageError::Install(
            "retained previous native version is not a directory".to_owned(),
        ));
    }

    let current_exists = match fs::symlink_metadata(&install.root) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(PackageError::Install(
                    "active native installation is not a directory".to_owned(),
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(PackageError::Io(error)),
    };
    let displaced = temporary_path(parent, &install.root, "rollback");
    if current_exists {
        if let Err(error) = fs::rename(&install.root, &displaced) {
            return Err(PackageError::Io(error));
        }
    }
    if let Err(error) = fs::rename(previous, &install.root) {
        if current_exists {
            let _ = fs::rename(&displaced, &install.root);
        }
        return Err(PackageError::Io(error));
    }
    if current_exists {
        fs::remove_dir_all(&displaced).map_err(PackageError::Io)?;
    }
    Ok(())
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest, PackageError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| PackageError::Manifest(format!("{MANIFEST_MEMBER} is not UTF-8: {error}")))?;
    Manifest::parse(text).map_err(|error| PackageError::Manifest(error.to_string()))
}

fn validate_manifest_shape(manifest: &Manifest) -> Result<(), PackageError> {
    // `c-abi` and `wasm` share this package format. Their payload is a shared
    // library or a `.wasm` module rather than a program, and the executable
    // CriKey supervises is `crikey-cabi-host` or `crikey-wasm-host`, but the
    // archive, the lock and the atomic install are identical. A second
    // nearly-identical format would be a second place to get member
    // authentication wrong (ADR-0014, ADR-0015).
    if !matches!(
        manifest.plugin.runtime,
        Runtime::Native | Runtime::CAbi | Runtime::Wasm
    ) {
        return Err(PackageError::Manifest(
            "native packages require plugin.runtime = \"native\", \"c-abi\" or \"wasm\"".to_owned(),
        ));
    }
    if manifest.plugin.id.is_empty() || manifest.plugin.version.is_empty() {
        return Err(PackageError::Manifest(
            "plugin id and version must not be empty".to_owned(),
        ));
    }
    safe_id_component(&manifest.plugin.id)?;
    if manifest.plugin.entrypoint.is_empty() {
        return Err(PackageError::Manifest(
            "native package manifest has no entrypoint".to_owned(),
        ));
    }
    Ok(())
}

fn validate_manifest_members<I, D>(manifest: &Manifest, names: I, directories: D) -> Result<(), PackageError>
where
    I: IntoIterator<Item = String>,
    D: IntoIterator<Item = String>,
{
    let names = names.into_iter().collect::<BTreeSet<_>>();
    let directories = directories.into_iter().collect::<BTreeSet<_>>();
    let binary_count = names
        .iter()
        .filter(|name| name.starts_with("bin/") && !name.ends_with('/') && !name.ends_with(".sig"))
        .count();
    if binary_count == 0 {
        return Err(PackageError::Manifest(
            "native package contains no bin/ payload".to_owned(),
        ));
    }
    for entrypoint in manifest.plugin.entrypoint.values() {
        if !names.contains(entrypoint) || directories.contains(entrypoint) {
            return Err(PackageError::Manifest(format!(
                "entrypoint {entrypoint:?} is not a package file"
            )));
        }
    }
    Ok(())
}

fn collect_source_members(
    plugin_dir: &Path,
    out: &Path,
) -> Result<BTreeMap<String, SourceMember>, PackageError> {
    let metadata = fs::symlink_metadata(plugin_dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(PackageError::Manifest(format!(
            "plugin path {} is not a directory",
            plugin_dir.display()
        )));
    }
    let output_existing = fs::canonicalize(out).ok();
    let mut pending = vec![plugin_dir.to_path_buf()];
    let mut members = BTreeMap::new();
    let mut remaining = MAX_UNPACKED_BYTES;
    while let Some(directory) = pending.pop() {
        let mut children = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|entry| entry.path());
        for entry in children {
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(PackageError::Manifest(format!(
                    "symbolic links are not allowed in native packages: {}",
                    path.display()
                )));
            }
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                return Err(PackageError::Manifest(format!(
                    "unsupported package member type: {}",
                    path.display()
                )));
            }
            let is_output = output_existing
                .as_ref()
                .and_then(|output| {
                    fs::canonicalize(&path)
                        .ok()
                        .map(|candidate| candidate.as_path() == output.as_path())
                })
                .unwrap_or(false);
            if is_output {
                continue;
            }
            let relative = path.strip_prefix(plugin_dir).map_err(|_| {
                PackageError::Manifest(format!("package member escaped {}", plugin_dir.display()))
            })?;
            let name = source_archive_name(relative)?;
            if name == LOCK_MEMBER {
                return Err(PackageError::Manifest(format!(
                    "{LOCK_MEMBER} is generated by the package builder"
                )));
            }
            let length = fs::metadata(&path)?.len();
            if length > remaining || members.len() >= MAX_MEMBERS {
                return Err(PackageError::Manifest(format!(
                    "{} exceeds the {MAX_UNPACKED_BYTES} byte / {MAX_MEMBERS} member package limit",
                    plugin_dir.display()
                )));
            }
            remaining -= length;
            let bytes = fs::read(&path)?;
            let unix_mode = file_mode(&path)?;
            if members
                .insert(name.clone(), SourceMember { bytes, unix_mode })
                .is_some()
            {
                return Err(PackageError::Manifest(format!("duplicate package member {name}")));
            }
        }
    }
    Ok(members)
}

/// Every member of a validated ZIP package, plus the hash of its original
/// archive bytes.
#[derive(Debug)]
pub(crate) struct ArchiveContents {
    pub(crate) members: BTreeMap<String, ArchiveMember>,
    pub(crate) hash: String,
}

fn source_archive_name(relative: &Path) -> Result<String, PackageError> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(PackageError::Manifest(format!(
                "unsafe package path {}",
                relative.display()
            )));
        };
        let part = part.to_str().ok_or_else(|| {
            PackageError::Manifest(format!("package path {} is not UTF-8", relative.display()))
        })?;
        if part.is_empty() || part.contains(['/', '\\', ':']) {
            return Err(PackageError::Manifest(format!(
                "unsafe package path {}",
                relative.display()
            )));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(PackageError::Manifest("empty package path".to_owned()));
    }
    Ok(parts.join("/"))
}

pub(crate) fn read_archive(archive: &Path) -> Result<ArchiveContents, PackageError> {
    let size = fs::metadata(archive)?.len();
    if size > MAX_PACKAGE_BYTES {
        return Err(PackageError::MalformedArchive(format!(
            "{} is {size} bytes, over the {MAX_PACKAGE_BYTES} byte package limit",
            archive.display()
        )));
    }
    let archive_bytes = fs::read(archive)?;
    let hash = sha256_hex(&archive_bytes);
    let mut zip = ZipArchive::new(Cursor::new(archive_bytes.as_slice()))
        .map_err(|error| PackageError::MalformedArchive(error.to_string()))?;
    if zip.len() > MAX_MEMBERS {
        return Err(PackageError::MalformedArchive(format!(
            "{} declares {} members, over the {MAX_MEMBERS} member limit",
            archive.display(),
            zip.len()
        )));
    }
    let mut members = BTreeMap::new();
    let mut remaining = MAX_UNPACKED_BYTES;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| PackageError::MalformedArchive(error.to_string()))?;
        let raw_name = entry.name_raw().to_vec();
        let name = std::str::from_utf8(&raw_name)
            .map_err(|error| PackageError::MalformedArchive(format!("entry name is not UTF-8: {error}")))?
            .to_owned();
        let directory = entry.is_dir() || name.ends_with('/');
        validate_archive_name(&name, directory)?;
        if entry.is_symlink() {
            return Err(PackageError::MalformedArchive(format!(
                "symbolic-link entry {name} is not allowed"
            )));
        }
        let mut bytes = Vec::new();
        let read = (&mut entry)
            .take(remaining + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| PackageError::MalformedArchive(format!("could not read {name}: {error}")))?;
        if (read as u64) > remaining {
            return Err(PackageError::MalformedArchive(format!(
                "{} expands to more than the {MAX_UNPACKED_BYTES} byte package limit",
                archive.display()
            )));
        }
        remaining -= read as u64;
        if directory && !bytes.is_empty() {
            return Err(PackageError::MalformedArchive(format!(
                "directory entry {name} has a payload"
            )));
        }
        if members
            .insert(
                name.clone(),
                ArchiveMember {
                    bytes,
                    directory,
                    unix_mode: entry.unix_mode(),
                },
            )
            .is_some()
        {
            return Err(PackageError::MalformedArchive(format!(
                "duplicate archive member {name}"
            )));
        }
    }
    validate_archive_prefixes(&members)?;
    Ok(ArchiveContents { members, hash })
}

/// Reads `MANIFEST_MEMBER` out of already-validated archive members.
pub(crate) fn archive_manifest(members: &BTreeMap<String, ArchiveMember>) -> Result<Manifest, PackageError> {
    let manifest_member = members
        .get(MANIFEST_MEMBER)
        .ok_or_else(|| PackageError::Manifest(format!("package is missing {MANIFEST_MEMBER}")))?;
    if manifest_member.directory {
        return Err(PackageError::Manifest(format!(
            "{MANIFEST_MEMBER} must be a file"
        )));
    }
    parse_manifest(&manifest_member.bytes)
}

fn load_package(archive: &Path) -> Result<LoadedPackage, PackageError> {
    let ArchiveContents { members, hash } = read_archive(archive)?;
    let manifest = archive_manifest(&members)?;
    validate_manifest_shape(&manifest)?;
    let names = members.keys().cloned().collect::<Vec<_>>();
    let directories = members
        .iter()
        .filter(|(_, member)| member.directory)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    validate_manifest_members(&manifest, names, directories)?;

    Ok(LoadedPackage {
        members,
        manifest,
        archive_hash: hash,
    })
}

fn validate_archive_name(name: &str, directory: bool) -> Result<(), PackageError> {
    let without_slash = if directory {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    if without_slash.is_empty()
        || name.as_bytes().contains(&0)
        || name.starts_with('/')
        || name.starts_with('\\')
        || (!directory && name.ends_with('/'))
    {
        return Err(PackageError::MalformedArchive(format!(
            "unsafe archive member path {name:?}"
        )));
    }
    for part in without_slash.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':']) {
            return Err(PackageError::MalformedArchive(format!(
                "unsafe archive member path {name:?}"
            )));
        }
    }
    Ok(())
}

fn validate_archive_prefixes(members: &BTreeMap<String, ArchiveMember>) -> Result<(), PackageError> {
    for name in members.keys() {
        let mut prefix = String::new();
        for (index, part) in name.split('/').enumerate() {
            if index > 0 {
                prefix.push('/');
            }
            prefix.push_str(part);
            if prefix.as_str() != name.as_str()
                && members.get(&prefix).is_some_and(|candidate| !candidate.directory)
            {
                return Err(PackageError::MalformedArchive(format!(
                    "archive member {name} conflicts with file {prefix}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_integrity(package: &LoadedPackage) -> Result<(), PackageError> {
    let lock_member = package
        .members
        .get(LOCK_MEMBER)
        .ok_or_else(|| PackageError::HashMismatch(format!("package is missing {LOCK_MEMBER}")))?;
    if lock_member.directory {
        return Err(PackageError::HashMismatch(format!(
            "{LOCK_MEMBER} is a directory"
        )));
    }
    let lock_text = std::str::from_utf8(&lock_member.bytes)
        .map_err(|error| PackageError::HashMismatch(format!("{LOCK_MEMBER} is not UTF-8: {error}")))?;
    let lock: PackageLock = toml::from_str(lock_text)
        .map_err(|error| PackageError::HashMismatch(format!("invalid {LOCK_MEMBER}: {error}")))?;
    let canonical_lock = toml::to_string(&lock)
        .map_err(|error| PackageError::HashMismatch(format!("could not encode {LOCK_MEMBER}: {error}")))?;
    if lock_text.as_bytes() != canonical_lock.as_bytes() {
        return Err(PackageError::HashMismatch(format!(
            "{LOCK_MEMBER} is not in canonical form"
        )));
    }
    if lock.plugin != package.manifest.plugin.id || lock.version != package.manifest.plugin.version {
        return Err(PackageError::HashMismatch(format!(
            "{LOCK_MEMBER} identity does not match crikey.toml"
        )));
    }

    let actual_names = package
        .members
        .keys()
        .filter(|name| name.as_str() != LOCK_MEMBER)
        .cloned()
        .collect::<BTreeSet<_>>();
    let expected_names = lock.entries.keys().cloned().collect::<BTreeSet<_>>();
    if actual_names != expected_names {
        return Err(PackageError::HashMismatch(format!(
            "{LOCK_MEMBER} does not list exactly every archive member"
        )));
    }
    for name in actual_names {
        let expected = lock
            .entries
            .get(&name)
            .ok_or_else(|| PackageError::HashMismatch(format!("{LOCK_MEMBER} has no digest for {name}")))?;
        let actual = sha256_hex(&package.members[&name].bytes);
        if !is_hex_sha256(expected) || !constant_time_hex_eq(expected, &actual) {
            return Err(PackageError::HashMismatch(format!(
                "digest mismatch for archive member {name}"
            )));
        }
    }
    Ok(())
}

fn ensure_compatible(manifest: &Manifest, os: &str, arch: &str) -> Result<(), PackageError> {
    let os_ok = manifest.platform.os.is_empty() || manifest.platform.os.iter().any(|value| value == os);
    let arch_ok =
        manifest.platform.arch.is_empty() || manifest.platform.arch.iter().any(|value| value == arch);
    if !os_ok || !arch_ok {
        return Err(PackageError::IncompatiblePlatform);
    }
    if manifest.entrypoint_for(os, arch).is_err() {
        return Err(PackageError::MissingEntrypoint {
            os: os.to_owned(),
            arch: arch.to_owned(),
        });
    }
    Ok(())
}

/// Whether any `bin/` payload ships without a sibling `<name>.sig` file.
///
/// A *shape* check, and nothing more: no signature file's contents are read
/// here, no key is consulted, and nothing is refused. It predates package
/// signing and is kept because `crikey plugin list` reports it, but it is not
/// provenance and must never be read as any.
///
/// Provenance is [`SignatureState`], established by
/// [`verify_package_with_policy`] from the detached `<package>.sig` beside the
/// archive and the operator's trust store (ADR 0012). The two facts are reported
/// under two names precisely so neither is mistaken for the other.
pub(crate) fn unsigned_binary_in(members: &BTreeMap<String, ArchiveMember>) -> bool {
    members.iter().any(|(name, member)| {
        if !name.starts_with("bin/") || member.directory || name.ends_with(".sig") {
            return false;
        }
        match members.get(&format!("{name}.sig")) {
            Some(signature) => signature.directory,
            None => true,
        }
    })
}

/// Reads a plugin source *directory* into the same member map an archive
/// yields, so a directory install and an archive install share one staging and
/// swap path (spec 23.1).
pub(crate) fn collect_directory_members(
    plugin_dir: &Path,
) -> Result<BTreeMap<String, ArchiveMember>, PackageError> {
    Ok(collect_source_members(plugin_dir, Path::new(""))?
        .into_iter()
        .map(|(name, member)| {
            (
                name,
                ArchiveMember {
                    bytes: member.bytes,
                    directory: false,
                    unix_mode: member.unix_mode,
                },
            )
        })
        .collect())
}

/// Re-authenticates one file inside an *installed* package directory against
/// the `crikey-package.lock` installation wrote beside it (spec 23.3, 23.4).
///
/// Installation already validated the whole archive, but a directory on disk
/// can be edited afterwards. A host that is about to load third-party code out
/// of that directory checks the one member it is about to load rather than
/// trusting the install that happened at some earlier date. The lock is
/// re-canonicalised exactly as [`inspect_package`] does, so a rewritten or
/// reordered lock is refused before any digest is compared.
///
/// This authenticates the *bytes*; it says nothing about who produced them.
/// The lock is not a signature.
pub fn verify_installed_member(package_dir: &Path, member: &str) -> Result<(), PackageError> {
    let lock_path = package_dir.join(LOCK_MEMBER);
    let lock_bytes = read_file_capped(&lock_path, MAX_PACKAGE_BYTES)?;
    let lock_text = std::str::from_utf8(&lock_bytes)
        .map_err(|error| PackageError::HashMismatch(format!("{LOCK_MEMBER} is not UTF-8: {error}")))?;
    let lock: PackageLock = toml::from_str(lock_text)
        .map_err(|error| PackageError::HashMismatch(format!("invalid {LOCK_MEMBER}: {error}")))?;
    let canonical_lock = toml::to_string(&lock)
        .map_err(|error| PackageError::HashMismatch(format!("could not encode {LOCK_MEMBER}: {error}")))?;
    if lock_text.as_bytes() != canonical_lock.as_bytes() {
        return Err(PackageError::HashMismatch(format!(
            "{LOCK_MEMBER} is not in canonical form"
        )));
    }
    let expected = lock
        .entries
        .get(member)
        .ok_or_else(|| PackageError::HashMismatch(format!("{LOCK_MEMBER} has no digest for {member}")))?;
    let bytes = read_file_capped(&package_dir.join(member), MAX_PACKAGE_BYTES)?;
    let actual = sha256_hex(&bytes);
    if !is_hex_sha256(expected) || !constant_time_hex_eq(expected, &actual) {
        return Err(PackageError::HashMismatch(format!(
            "digest mismatch for installed member {member}"
        )));
    }
    Ok(())
}

/// Reads a whole file, refusing anything past `limit` instead of allocating
/// it. The read itself is capped rather than the metadata length: a file that
/// grows between the two is refused, not truncated.
fn read_file_capped(path: &Path, limit: u64) -> Result<Vec<u8>, PackageError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(PackageError::Install(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(PackageError::Install(format!(
            "{} is larger than {limit} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn package_report(package: &LoadedPackage, signature: SignatureState) -> NativePackageReport {
    let unsigned_binary = unsigned_binary_in(&package.members);
    NativePackageReport {
        plugin: package.manifest.plugin.id.clone(),
        version: package.manifest.plugin.version.clone(),
        os: package.manifest.platform.os.clone(),
        arch: package.manifest.platform.arch.clone(),
        entries: package
            .members
            .iter()
            .map(|(name, member)| (name.clone(), member.bytes.len() as u64))
            .collect(),
        hash: package.archive_hash.clone(),
        signature,
        unsigned_binary,
    }
}

/// Materialises `members` under `staging`, which the caller then swaps into
/// place. Nothing is written where the installation lives, so an interrupted
/// write leaves the installed version untouched.
pub(crate) fn write_members(
    members: &BTreeMap<String, ArchiveMember>,
    staging: &Path,
) -> Result<(), PackageError> {
    fs::create_dir_all(staging)?;
    for (name, member) in members {
        let destination = staging.join(name);
        if member.directory {
            fs::create_dir_all(&destination)?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&destination)?;
        file.write_all(&member.bytes)?;
        #[cfg(unix)]
        if let Some(mode) = member.unix_mode {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(mode & 0o7777))?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallRootState {
    Missing,
    Empty,
    Populated,
}

fn inspect_install_root(root: &Path) -> Result<InstallRootState, PackageError> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InstallRootState::Missing);
        }
        Err(error) => return Err(PackageError::Io(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PackageError::Install(format!(
            "install root {} is not a directory",
            root.display()
        )));
    }
    let mut entries = fs::read_dir(root)?;
    if entries.next().transpose()?.is_none() {
        Ok(InstallRootState::Empty)
    } else {
        Ok(InstallRootState::Populated)
    }
}

pub(crate) fn install_parent(root: &Path) -> Result<&Path, PackageError> {
    root.parent()
        .ok_or_else(|| PackageError::Install(format!("install root {} has no parent", root.display())))
}

pub(crate) fn temporary_path(parent: &Path, root: &Path, kind: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = root
        .file_name()
        .map_or_else(|| "native".to_owned(), |name| name.to_string_lossy().into_owned());
    loop {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{base}.{kind}-{}-{sequence}", std::process::id()));
        if !candidate.exists() {
            return candidate;
        }
    }
}

fn remove_directory_if_present(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn file_mode(path: &Path) -> Result<Option<u32>, PackageError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(Some(fs::metadata(path)?.permissions().mode()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn archive_options(mode: Option<u32>) -> SimpleFileOptions {
    let mut options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    if let Some(mode) = mode {
        options = options.unix_permissions(mode);
    }
    options
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn is_hex_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
