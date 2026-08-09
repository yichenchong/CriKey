//! Executable contract for legacy package discovery and loading (spec 14.3,
//! 14.8, 26.2; acceptance 31.11; roadmap M3).
//!
//! Spec 14.3 requires CriKey to load loose Keypirinha package directories,
//! `.keypirinha-package` archives, package-local Python modules, package
//! resources and Keypirinha-style configuration files. This file defends the
//! discovery and loading half of that requirement: what a package *is*, how it
//! is named, what it exposes, and — the part that actually matters — what it
//! refuses. Reading a configuration file is `LegacySettings`' job (src/config.rs);
//! the loader only reports *where* the file is, as a resource.
//!
//! The model these tests pin:
//!
//! * A package is a directory, or a ZIP archive named `*.keypirinha-package`
//!   whose entries are package-root-relative with no wrapping folder.
//! * The package id is the file stem of that directory or archive, verbatim —
//!   no case folding, no normalization — so the same package has the same
//!   identity whether it ships loose or zipped. Paths are never canonicalized:
//!   `LegacyPackage::root` reports exactly the path the caller supplied, which
//!   is what makes a compatibility diagnostic (spec 26.2) point at something
//!   the user recognizes.
//! * Every `.py` file is an importable package-local module, named by its
//!   path: `lib/helpers.py` is `lib.helpers`, and `lib/__init__.py` is `lib`.
//!   Everything else — icons, data files, `.ini` configuration — is a resource.
//!   The two sets are disjoint and both are sorted, because a plugin's import
//!   set must not depend on `read_dir` order.
//! * The main plugin module is the top-level module whose stem equals the
//!   package id; failing that, the lexicographically first top-level module.
//!   It is always also present in `modules`.
//! * A package with no top-level module is not an empty package, it is a
//!   broken one, and it is refused by name.
//!
//! Extraction is a cache concern, not a package concern. An archive extracts
//! under `PackageLoader::cache_root()`, deterministically (the same archive
//! yields the same `extracted` path every time) and idempotently (reloading
//! neither duplicates entries nor re-serves stale content after the archive on
//! disk changes). The recommended mechanism is a content-addressed extraction
//! directory; these tests pin the property, not the mechanism.
//!
//! Refusal is the security surface. An archive is hostile input: it arrives
//! from the internet and its entry names are attacker-controlled. The tests
//! cover path escapes and absolute names, backslash separators, symbolic
//! links, duplicate names, invalid UTF-8, empty archives, entry-count limits,
//! and decompressed-size caps. Each asserts the filesystem afterwards rather
//! than trusting the returned `Err`:
//!
//! 1. No entry escapes the extraction directory (`../escape.py`).
//! 2. No entry name that is not valid UTF-8 is lossily decoded into a path.
//! 3. No entry exceeds the loader's documented size cap.
//!
//! All three require the loader to validate *every* entry name before writing
//! *any* byte — a loader that extracts as it scans has already lost by the time
//! it notices. The tests therefore assert that the cache root is completely
//! empty after a refusal, and the traversal test additionally walks the whole
//! temp tree looking for the escaped file.
//!
//! Fixture policy. Well-formed archives are built with the `zip` crate, so the
//! loader is proven against a mainstream writer rather than only against this
//! file's idea of a ZIP. The hostile archives are hand-written raw bytes
//! (`RawZip`, stored entries only): a security fixture must not be produced by
//! a writer that is free to sanitize it, and `zip`'s API cannot express a
//! non-UTF-8 entry name at all: `start_file` is bound by `S: ToString`, so the
//! name is always a Rust `String`, hence always valid UTF-8.
//!
//! Every fixture lives in a unique directory under `std::env::temp_dir()` and
//! is removed by an RAII guard. Nothing here sleeps, reads a clock, opens a
//! socket, or touches `compatibility/test-plugins/`.

use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::symlink;

use crikey_legacy_compat::{
    LegacyPackage, PackageError, PackageId, PackageLimits, PackageLoader, PackageModule, PackageRoot,
    PACKAGE_ARCHIVE_EXTENSION,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

// ---------------------------------------------------------------------------
// Fixture layouts
//
// A layout is `(package-relative path, contents)`. The same layout builds both
// a loose directory and an archive, so the equivalence test compares like with
// like instead of comparing two hand-transcribed expectations.
// ---------------------------------------------------------------------------

type FixtureFile = (&'static str, &'static [u8]);

/// Exercises every classification rule at once: a main module matching the
/// package id, an `__init__.py` package, a nested module, a binary resource, a
/// resource in a subdirectory, and a Keypirinha-style configuration file.
const EVERYTHING: &[FixtureFile] = &[
    ("Everything.py", b"import keypirinha\n"),
    ("lib/__init__.py", b""),
    ("lib/helpers.py", b"HELPER = 1\n"),
    ("lib/nested/deep.py", b"DEEP = 2\n"),
    ("icon.png", b"\x89PNG\r\n\x1a\n not really a png"),
    ("data/list.txt", b"one\ntwo\n"),
    ("everything.ini", b"[main]\nenabled = yes\n"),
];

/// Import names for [`EVERYTHING`], in the order the loader must report them.
const EVERYTHING_MODULES: &[&str] = &["Everything", "lib", "lib.helpers", "lib.nested.deep"];

/// A package whose only top-level `.py` file is its main module.
const MINIMAL: &[FixtureFile] = &[("hello.py", b"import keypirinha\n")];

// ---------------------------------------------------------------------------
// Temp tree
// ---------------------------------------------------------------------------

static NEXT_TREE: AtomicU64 = AtomicU64::new(0);

/// A unique temp directory, removed when the test ends.
#[derive(Debug)]
struct TempTree {
    dir: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let unique = NEXT_TREE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "crikey-legacy-package-{pid}-{unique}-{label}",
            pid = std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp tree root must be creatable");
        Self { dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    /// The loader's cache root.
    ///
    /// Deliberately *not* created: extraction must create it on demand. It also
    /// sits two levels below the guard root on purpose, so that a naive
    /// extraction of `../escape.py` or `../../escape.py` still lands inside the
    /// guarded tree where the traversal test can see it — and where the guard
    /// will clean it up — instead of escaping into the shared temp directory.
    fn cache_root(&self) -> PathBuf {
        self.dir.join("cache").join("legacy")
    }

    /// A directory to be handed to `discover` as a package root.
    fn root(&self, name: &str) -> PathBuf {
        let path = self.dir.join("roots").join(name);
        fs::create_dir_all(&path).expect("package root must be creatable");
        path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        restore_permissions(&self.dir);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Re-grants owner access to every directory in the tree so a fixture that
/// deliberately made a directory unreadable can still be removed. Without this
/// the mode-`0o000` root in `a_missing_unreadable_or_non_directory_root_is_skipped`
/// would leak a temp directory on every run.
#[cfg(unix)]
fn restore_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        restore_permissions(&entry.path());
    }
}

#[cfg(not(unix))]
fn restore_permissions(_path: &Path) {}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn write_file(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("fixture parent {} must be creatable: {error}", parent.display()));
    }
    fs::write(path, contents)
        .unwrap_or_else(|error| panic!("fixture file {} must be writable: {error}", path.display()));
}

/// Materializes `files` as a loose package directory `<root>/<name>` and
/// returns the package directory.
fn build_directory_package(root: &Path, name: &str, files: &[FixtureFile]) -> PathBuf {
    let package = root.join(name);
    fs::create_dir_all(&package)
        .unwrap_or_else(|error| panic!("package dir {} must be creatable: {error}", package.display()));
    for (relative, contents) in files {
        write_file(&package.join(relative), contents);
    }
    package
}

/// Every entry below `root`, as `(package-relative path, is_dir)`, sorted.
fn walk_entries(root: &Path) -> Vec<(PathBuf, bool)> {
    fn collect(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, bool)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("walked path must live under the walked root")
                .to_path_buf();
            let is_dir = path.is_dir();
            out.push((relative, is_dir));
            if is_dir {
                collect(root, &path, out);
            }
        }
    }

    let mut out = Vec::new();
    collect(root, root, &mut out);
    out.sort();
    out
}

/// Every *file* below `root`, relative to it, sorted. A missing root is an
/// empty tree, which is exactly what the refusal tests want to assert.
fn relative_files(root: &Path) -> Vec<PathBuf> {
    walk_entries(root)
        .into_iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(relative, _)| relative)
        .collect()
}

/// Absolute paths of every file named `file_name` anywhere below `root`.
fn find_named(root: &Path, file_name: &str) -> Vec<PathBuf> {
    relative_files(root)
        .into_iter()
        .filter(|relative| relative.file_name().and_then(|name| name.to_str()) == Some(file_name))
        .map(|relative| root.join(relative))
        .collect()
}

// ---------------------------------------------------------------------------
// Archive fixtures
// ---------------------------------------------------------------------------

fn archive_file_name(name: &str) -> String {
    format!("{name}.{PACKAGE_ARCHIVE_EXTENSION}")
}

/// Slash-separated ZIP entry name for a package-relative path.
fn zip_entry_name(relative: &Path, is_dir: bool) -> String {
    let mut name = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    if is_dir {
        name.push('/');
    }
    name
}

/// Zips a loose package directory into a well-formed `.keypirinha-package`.
///
/// Directory entries are written explicitly and `.py` files are deflated while
/// everything else is stored, so a single fixture proves the loader ignores
/// directory entries and reads both compression methods.
fn archive_from_directory(source: &Path, archive: &Path) {
    if let Some(parent) = archive.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("archive parent {} must exist: {error}", parent.display()));
    }
    let file = fs::File::create(archive)
        .unwrap_or_else(|error| panic!("archive {} must be creatable: {error}", archive.display()));
    let mut writer = ZipWriter::new(file);

    for (relative, is_dir) in walk_entries(source) {
        let name = zip_entry_name(&relative, is_dir);
        if is_dir {
            writer
                .add_directory(name, SimpleFileOptions::default())
                .expect("directory entry must be writable");
            continue;
        }
        let bytes = fs::read(source.join(&relative))
            .unwrap_or_else(|error| panic!("fixture {} must be readable: {error}", relative.display()));
        let method = if relative.extension().and_then(|ext| ext.to_str()) == Some("py") {
            CompressionMethod::Deflated
        } else {
            CompressionMethod::Stored
        };
        writer
            .start_file(name, SimpleFileOptions::default().compression_method(method))
            .expect("file entry must be writable");
        writer.write_all(&bytes).expect("entry payload must be writable");
    }

    writer.finish().expect("archive must be finalizable");

    // Fixture self-check. The loader is required to ignore ZIP directory
    // entries, which is only a meaningful claim if the fixture actually
    // contains some. Verifying it here means a change in how `zip` writes
    // directory entries fails as a fixture error, right where it happened,
    // instead of surfacing later as a mysterious extra "resource".
    let written = fs::File::open(archive)
        .unwrap_or_else(|error| panic!("archive {} must be re-openable: {error}", archive.display()));
    let mut reader = ZipArchive::new(written).unwrap_or_else(|error| {
        panic!(
            "fixture archive {} must be a valid ZIP: {error}",
            archive.display()
        )
    });
    let mut names = Vec::with_capacity(reader.len());
    for index in 0..reader.len() {
        names.push(
            reader
                .by_index(index)
                .expect("every entry just written must be readable back")
                .name()
                .to_owned(),
        );
    }
    for (relative, _) in walk_entries(source).into_iter().filter(|(_, is_dir)| *is_dir) {
        let expected = format!("{}/", zip_entry_name(&relative, false));
        assert!(
            names.contains(&expected),
            "fixture archive must carry the directory entry {expected:?} so the loader is genuinely \
             tested against one, got {names:?}"
        );
    }
}

/// A minimal ZIP writer emitting stored entries with raw, uninterpreted entry
/// name bytes.
///
/// This exists because the hostile fixtures cannot be expressed with the `zip`
/// crate: `ZipWriter::start_file` is bound by `S: ToString`, so an entry name is
/// always a Rust `String` and a non-UTF-8 one is unrepresentable. A security
/// fixture must also not depend on a third-party writer's sanitization policy —
/// if that policy tightened the fixture would quietly stop being hostile, and
/// the test would pass for the wrong reason.
#[derive(Debug, Default)]
struct RawZip {
    entries: Vec<(Vec<u8>, Vec<u8>, u32)>,
}

impl RawZip {
    /// General purpose bit 11: entry names are declared UTF-8. Set on every
    /// entry, so a name that is *not* valid UTF-8 is unambiguously malformed
    /// rather than a legal CP437 name open to interpretation.
    const UTF8_NAME_FLAG: u16 = 0x0800;
    /// MS-DOS timestamp for 1980-01-01, the earliest ZIP can represent.
    const DOS_DATE: u16 = 0x0021;

    fn push(&mut self, name: impl Into<Vec<u8>>, data: impl Into<Vec<u8>>) -> &mut Self {
        self.entries.push((name.into(), data.into(), 0));
        self
    }

    fn push_symlink(&mut self, name: impl Into<Vec<u8>>, target: impl Into<Vec<u8>>) -> &mut Self {
        // Unix mode S_IFLNK | 0777, stored in the high half of the central
        // directory's external-attributes field.
        self.entries
            .push((name.into(), target.into(), (0o120777u32) << 16));
        self
    }

    fn write_to(&self, path: &Path) {
        let mut body = Vec::new();
        let mut central = Vec::new();

        for (name, data, external_attributes) in &self.entries {
            let crc = crc32(data);
            let offset = body.len() as u32;
            let size = data.len() as u32;

            body.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header signature
            body.extend_from_slice(&20u16.to_le_bytes()); // version needed
            body.extend_from_slice(&Self::UTF8_NAME_FLAG.to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            body.extend_from_slice(&0u16.to_le_bytes()); // modification time
            body.extend_from_slice(&Self::DOS_DATE.to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes());
            body.extend_from_slice(&size.to_le_bytes()); // compressed size
            body.extend_from_slice(&size.to_le_bytes()); // uncompressed size
            body.extend_from_slice(&(name.len() as u16).to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes()); // extra field length
            body.extend_from_slice(name);
            body.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header signature
            central.extend_from_slice(&((3u16 << 8) | 20).to_le_bytes()); // Unix version made by
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&Self::UTF8_NAME_FLAG.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            central.extend_from_slice(&0u16.to_le_bytes()); // modification time
            central.extend_from_slice(&Self::DOS_DATE.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes()); // compressed size
            central.extend_from_slice(&size.to_le_bytes()); // uncompressed size
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra field length
            central.extend_from_slice(&0u16.to_le_bytes()); // comment length
            central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            central.extend_from_slice(&external_attributes.to_le_bytes()); // external attributes
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
        }

        let central_offset = body.len() as u32;
        let central_size = central.len() as u32;
        let count = self.entries.len() as u16;

        body.extend_from_slice(&central);
        body.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // end of central directory
        body.extend_from_slice(&0u16.to_le_bytes()); // this disk
        body.extend_from_slice(&0u16.to_le_bytes()); // disk with central directory
        body.extend_from_slice(&count.to_le_bytes());
        body.extend_from_slice(&count.to_le_bytes());
        body.extend_from_slice(&central_size.to_le_bytes());
        body.extend_from_slice(&central_offset.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // archive comment length

        write_file(path, &body);
    }
}

/// CRC-32/ISO-HDLC over the uncompressed entry payload, as ZIP requires.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

fn load_ok(loader: &PackageLoader, path: &Path) -> LegacyPackage {
    loader
        .load(path)
        .unwrap_or_else(|error| panic!("load({}) must succeed: {error:?}", path.display()))
}

fn load_err(loader: &PackageLoader, path: &Path) -> PackageError {
    match loader.load(path) {
        Ok(package) => panic!(
            "load({}) must be refused, but yielded package {:?} with modules {:?}",
            path.display(),
            package.id,
            import_names(&package)
        ),
        Err(error) => error,
    }
}

fn discover_ok(loader: &PackageLoader, roots: &[PathBuf]) -> Vec<LegacyPackage> {
    loader
        .discover(roots)
        .unwrap_or_else(|error| panic!("discover({roots:?}) must succeed: {error:?}"))
}

fn ids(packages: &[LegacyPackage]) -> Vec<&str> {
    packages.iter().map(|package| package.id.0.as_str()).collect()
}

fn import_names(package: &LegacyPackage) -> Vec<&str> {
    package
        .modules
        .iter()
        .map(|module| module.import_name.as_str())
        .collect()
}

fn resource_paths(package: &LegacyPackage) -> Vec<&Path> {
    package.resources.iter().map(PathBuf::as_path).collect()
}

/// Compile-time proof that `PackageError` is a real `std::error::Error`, and
/// the accessor the diagnostics layer (spec 26.2) will render through.
fn as_std_error(error: &PackageError) -> &dyn std::error::Error {
    error
}

/// Renders `error` the way the diagnostics layer will and requires it to name
/// the thing that went wrong. A refusal nobody can act on is barely better than
/// a silent one (spec 26.2).
fn assert_error_names(error: &PackageError, needle: &str) {
    let rendered = as_std_error(error).to_string();
    assert!(
        rendered.contains(needle),
        "the rendered error must name {needle:?} so the compatibility diagnostic is actionable, \
         got {rendered:?}"
    );
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .expect("fixture paths always have a file name")
        .to_string_lossy()
        .into_owned()
}

/// Invariants that hold for *every* successfully loaded package, checked
/// alongside whatever a given test is actually about.
fn assert_package_invariants(package: &LegacyPackage) {
    let names = import_names(package);
    assert!(
        names.contains(&package.main_module.as_str()),
        "the main module {main:?} must also appear in the importable module set {names:?}",
        main = package.main_module
    );
    assert!(
        package.modules.is_sorted(),
        "modules must be reported in a deterministic sorted order, got {names:?}"
    );
    assert!(
        package.resources.is_sorted(),
        "resources must be reported in a deterministic sorted order, got {:?}",
        resource_paths(package)
    );

    let unique_modules: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(
        unique_modules.len(),
        names.len(),
        "no module may be reported twice, got {names:?}"
    );
    let unique_resources: BTreeSet<&Path> = resource_paths(package).into_iter().collect();
    assert_eq!(
        unique_resources.len(),
        package.resources.len(),
        "no resource may be reported twice, got {:?}",
        resource_paths(package)
    );

    for module in &package.modules {
        assert_eq!(
            module.relative_path.extension().and_then(|ext| ext.to_str()),
            Some("py"),
            "module {name:?} must resolve to a .py file, got {path}",
            name = module.import_name,
            path = module.relative_path.display()
        );
    }
    for resource in &package.resources {
        assert_ne!(
            resource.extension().and_then(|ext| ext.to_str()),
            Some("py"),
            "a .py file is an importable module, never a resource: {}",
            resource.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Loose directories
// ---------------------------------------------------------------------------

#[test]
fn a_loose_directory_with_a_top_level_module_is_discovered_with_its_id_main_module_and_resources() {
    let tree = TempTree::new("loose-directory");
    let root = tree.root("packages");
    let package_dir = build_directory_package(&root, "hello", MINIMAL);
    write_file(&package_dir.join("icon.png"), b"\x89PNG\r\n\x1a\n");

    let loader = PackageLoader::new(tree.cache_root());
    let discovered = discover_ok(&loader, std::slice::from_ref(&root));

    assert_eq!(
        ids(&discovered),
        vec!["hello"],
        "a loose directory holding a top-level .py module is a package, and its id is the directory name"
    );

    let package = &discovered[0];
    assert_package_invariants(package);
    assert_eq!(
        package.id,
        PackageId("hello".to_owned()),
        "the package id is the directory file stem, verbatim"
    );
    assert_eq!(
        package.root,
        PackageRoot::Directory(package_dir.clone()),
        "a loose package reports its directory as-is; the loader must not canonicalize the path"
    );
    assert_eq!(
        package.root.content_root(),
        package_dir.as_path(),
        "content_root() resolves package-relative module and resource paths for a loose directory"
    );
    assert_eq!(
        package.main_module, "hello",
        "the main plugin module is the top-level module whose stem matches the package id"
    );
    assert_eq!(
        package.modules,
        vec![PackageModule {
            import_name: "hello".to_owned(),
            relative_path: PathBuf::from("hello.py"),
        }],
        "the top-level plugin module is reported as an importable module with its package-relative path"
    );
    assert_eq!(
        resource_paths(package),
        vec![Path::new("icon.png")],
        "non-.py files alongside the plugin module are package resources"
    );

    assert_eq!(
        load_ok(&loader, &package_dir),
        *package,
        "load() of a package directory must yield exactly what discover() reports for it"
    );
}

#[test]
fn a_loose_directory_whose_name_looks_like_an_archive_keeps_its_full_directory_id() {
    let tree = TempTree::new("directory-extension-id");
    let root = tree.root("packages");
    let package_dir = build_directory_package(&root, "named.keypirinha-package", &[("entry.py", b"")]);

    let loader = PackageLoader::new(tree.cache_root());
    let package = load_ok(&loader, &package_dir);

    assert_eq!(
        package.id,
        PackageId("named.keypirinha-package".to_owned()),
        "only archive files lose the .keypirinha-package suffix; a loose directory name is \
         already the package identity"
    );
    assert_eq!(package.main_module, "entry");
}

#[test]
fn package_local_modules_in_subdirectories_are_importable_entries_in_sorted_order() {
    const LAYOUT: &[FixtureFile] = &[
        ("sortedmods.py", b"import keypirinha\n"),
        ("lib/__init__.py", b""),
        ("lib/b.py", b"B = 1\n"),
        ("lib/a.py", b"A = 1\n"),
        ("lib/nested/deep.py", b"DEEP = 1\n"),
    ];

    let tree = TempTree::new("sorted-modules");
    let root = tree.root("packages");
    let package_dir = build_directory_package(&root, "sortedmods", LAYOUT);

    let loader = PackageLoader::new(tree.cache_root());
    let package = load_ok(&loader, &package_dir);
    assert_package_invariants(&package);

    assert_eq!(
        import_names(&package),
        vec!["lib", "lib.a", "lib.b", "lib.nested.deep", "sortedmods"],
        "package-local modules in subdirectories are importable entries, dotted by path, sorted by \
         import name so the set never depends on read_dir order"
    );
    assert_eq!(
        package
            .modules
            .iter()
            .map(|module| module.relative_path.as_path())
            .collect::<Vec<_>>(),
        vec![
            Path::new("lib/__init__.py"),
            Path::new("lib/a.py"),
            Path::new("lib/b.py"),
            Path::new("lib/nested/deep.py"),
            Path::new("sortedmods.py"),
        ],
        "each module carries the package-relative path it loads from; lib/__init__.py is the module `lib`"
    );
    assert_eq!(
        package.main_module, "sortedmods",
        "subdirectory modules never displace the top-level plugin module"
    );

    let reloaded = load_ok(&loader, &package_dir);
    assert_eq!(
        reloaded.modules, package.modules,
        "module order is deterministic: two loads of the same directory must agree exactly"
    );
}

#[test]
fn non_python_files_are_reported_as_resources_and_never_as_modules() {
    let tree = TempTree::new("resources");
    let root = tree.root("packages");
    let package_dir = build_directory_package(&root, "Everything", EVERYTHING);

    let loader = PackageLoader::new(tree.cache_root());
    let package = load_ok(&loader, &package_dir);
    assert_package_invariants(&package);

    assert_eq!(
        resource_paths(&package),
        vec![
            Path::new("data/list.txt"),
            Path::new("everything.ini"),
            Path::new("icon.png"),
        ],
        "icons, data files and Keypirinha-style configuration files are resources, reported with \
         package-relative paths in sorted order (spec 14.3)"
    );
    assert_eq!(
        import_names(&package),
        EVERYTHING_MODULES,
        "resources must not leak into the importable module set"
    );

    let modules_as_paths: BTreeSet<&Path> = package
        .modules
        .iter()
        .map(|module| module.relative_path.as_path())
        .collect();
    let resources_as_paths: BTreeSet<&Path> = resource_paths(&package).into_iter().collect();
    assert!(
        modules_as_paths.is_disjoint(&resources_as_paths),
        "the module set and the resource set must be disjoint, got modules {modules_as_paths:?} \
         and resources {resources_as_paths:?}"
    );
}

// ---------------------------------------------------------------------------
// Archives
// ---------------------------------------------------------------------------

#[test]
fn an_archive_loads_to_the_same_logical_package_as_the_equivalent_loose_directory() {
    let tree = TempTree::new("archive-equivalence");
    let loose_root = tree.root("loose");
    let archive_root = tree.root("archived");

    let loose_dir = build_directory_package(&loose_root, "Everything", EVERYTHING);
    let archive_path = archive_root.join(archive_file_name("Everything"));
    archive_from_directory(&loose_dir, &archive_path);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::new(cache_root.clone());

    let loose = load_ok(&loader, &loose_dir);
    let archived = load_ok(&loader, &archive_path);
    assert_package_invariants(&loose);
    assert_package_invariants(&archived);

    assert_eq!(
        archived.id, loose.id,
        "a `.{PACKAGE_ARCHIVE_EXTENSION}` archive and the equivalent loose directory are the same \
         package: the id is the file stem either way"
    );
    assert_eq!(
        archived.main_module, loose.main_module,
        "packaging format must not change which module is the plugin entry point"
    );
    assert_eq!(
        archived.modules, loose.modules,
        "an archive exposes the identical importable module set, with identical package-relative paths"
    );
    assert_eq!(
        archived.resources, loose.resources,
        "an archive exposes the identical resource set; ZIP directory entries are not resources"
    );

    assert_eq!(
        loose.root,
        PackageRoot::Directory(loose_dir.clone()),
        "the loose package reports the directory it was loaded from"
    );
    let PackageRoot::Archive { archive, extracted } = &archived.root else {
        panic!(
            "a `.{PACKAGE_ARCHIVE_EXTENSION}` file must load as PackageRoot::Archive, got {:?}",
            archived.root
        );
    };
    assert_eq!(
        archive, &archive_path,
        "the archive root remembers the archive it came from, uncanonicalized, for diagnostics"
    );
    assert!(
        extracted.starts_with(&cache_root),
        "archive extraction must stay under the loader's cache root {cache}, got {extracted}",
        cache = cache_root.display(),
        extracted = extracted.display()
    );
    assert_eq!(
        archived.root.content_root(),
        extracted.as_path(),
        "content_root() of an archive package resolves against the extraction directory"
    );

    for resource in &loose.resources {
        assert_eq!(
            fs::read(archived.root.content_root().join(resource)).ok(),
            fs::read(loose.root.content_root().join(resource)).ok(),
            "resource {} must have identical bytes whether it ships loose or zipped",
            resource.display()
        );
    }
    for module in &loose.modules {
        assert_eq!(
            fs::read(archived.root.content_root().join(&module.relative_path)).ok(),
            fs::read(loose.root.content_root().join(&module.relative_path)).ok(),
            "module {} must have identical bytes whether it ships loose or zipped",
            module.import_name
        );
    }
}

#[test]
fn repeated_loads_of_the_same_archive_reuse_the_extraction_without_duplicating_entries() {
    const FIRST: &[FixtureFile] = &[("reused.py", b"VERSION = 1\n"), ("data/one.txt", b"first\n")];
    const SECOND: &[FixtureFile] = &[
        ("reused.py", b"VERSION = 2\n"),
        (
            "data/one.txt",
            b"second revision, materially longer than the first\n",
        ),
        ("extra.py", b"EXTRA = True\n"),
    ];

    let tree = TempTree::new("archive-reuse");
    let staging = tree.root("staging");
    let archive_path = tree.root("archived").join(archive_file_name("reused"));

    archive_from_directory(&build_directory_package(&staging, "v1", FIRST), &archive_path);

    let loader = PackageLoader::new(tree.cache_root());
    let first = load_ok(&loader, &archive_path);
    assert_package_invariants(&first);
    let extracted = first.root.content_root().to_path_buf();
    let after_first_load = relative_files(&extracted);

    let second = load_ok(&loader, &archive_path);
    assert_package_invariants(&second);

    assert_eq!(
        first.root, second.root,
        "extraction is deterministic: the same archive must extract to the same directory every load"
    );
    assert_eq!(
        import_names(&first),
        vec!["reused"],
        "the archive exposes exactly its own module — never anything the extraction cache added"
    );
    assert_eq!(
        resource_paths(&first),
        vec![Path::new("data/one.txt")],
        "the archive exposes exactly its own resource; cache bookkeeping must never surface as one"
    );
    assert_eq!(
        first.modules, second.modules,
        "reloading must not duplicate or reorder module entries"
    );
    assert_eq!(
        first.resources, second.resources,
        "reloading must not duplicate or reorder resource entries"
    );
    assert_eq!(
        relative_files(&extracted),
        after_first_load,
        "reusing an extraction must not accumulate files in the extraction directory"
    );
    assert!(
        after_first_load.contains(&PathBuf::from("data/one.txt"))
            && after_first_load.contains(&PathBuf::from("reused.py")),
        "the extraction directory must actually hold the archive's contents, found {after_first_load:?}"
    );
    assert_eq!(
        fs::read(extracted.join("data/one.txt")).ok(),
        Some(b"first\n".to_vec()),
        "reusing an extraction must serve the archive's actual bytes"
    );

    // Reuse must not become staleness: the archive is replaced in place.
    archive_from_directory(&build_directory_package(&staging, "v2", SECOND), &archive_path);
    let third = load_ok(&loader, &archive_path);
    assert_package_invariants(&third);

    assert_eq!(
        third.id, first.id,
        "rewriting an archive in place does not change the package id"
    );
    assert!(
        import_names(&third).contains(&"extra"),
        "an extraction may only be reused while it still matches the archive on disk; after the \
         archive changed, load() must report the new module set, got {:?}",
        import_names(&third)
    );
    assert_eq!(
        fs::read(third.root.content_root().join("data/one.txt")).ok(),
        Some(SECOND[1].1.to_vec()),
        "a reused extraction must never serve stale resource bytes after the archive changed"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_at_the_content_addressed_cache_path_is_not_reused() {
    let tree = TempTree::new("cache-symlink");
    let staging = tree.root("staging");
    let archive_path = tree.root("archived").join(archive_file_name("cached"));
    let package_dir = build_directory_package(
        &staging,
        "cached",
        &[("cached.py", b"import keypirinha\n"), ("data.txt", b"package\n")],
    );
    archive_from_directory(&package_dir, &archive_path);

    let loader = PackageLoader::new(tree.cache_root());
    let first = load_ok(&loader, &archive_path);
    let extracted = first.root.content_root().to_path_buf();
    fs::remove_dir_all(&extracted).expect("the first extraction must be a real directory");

    let outside = tree.path().join("outside");
    write_file(&outside.join("sentinel.txt"), b"outside\n");
    symlink(&outside, &extracted).expect("the hostile cache symlink must be creatable");

    let second = load_ok(&loader, &archive_path);
    let metadata = fs::symlink_metadata(second.root.content_root())
        .expect("the loader must leave a cache directory at the expected path");
    assert!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "a symlink at the content-addressed path must not be accepted as an extraction"
    );
    assert_eq!(
        fs::read(second.root.content_root().join("data.txt")).ok(),
        Some(b"package\n".to_vec()),
        "cache reuse must serve the archive's bytes, not the target of a planted symlink"
    );
    assert_eq!(
        fs::read(outside.join("sentinel.txt")).ok(),
        Some(b"outside\n".to_vec()),
        "re-extraction must not write through a cache symlink"
    );
}

// ---------------------------------------------------------------------------
// Discovery over multiple roots
// ---------------------------------------------------------------------------

#[test]
fn discovery_orders_packages_by_root_precedence_and_keeps_the_first_package_with_a_duplicate_id() {
    const SHARED_FIRST: &[FixtureFile] = &[("shared.py", b"WHICH = 1\n"), ("from-first.txt", b"first\n")];
    const SHARED_SECOND: &[FixtureFile] = &[("shared.py", b"WHICH = 2\n"), ("from-second.txt", b"second\n")];

    let tree = TempTree::new("root-precedence");
    let first = tree.root("first");
    let second = tree.root("second");

    build_directory_package(&first, "alpha", &[("alpha.py", b"import keypirinha\n")]);
    let shared_in_first = build_directory_package(&first, "shared", SHARED_FIRST);
    build_directory_package(&second, "beta", &[("beta.py", b"import keypirinha\n")]);
    let shared_in_second = build_directory_package(&second, "shared", SHARED_SECOND);

    let loader = PackageLoader::new(tree.cache_root());

    let forward = discover_ok(&loader, &[first.clone(), second.clone()]);
    assert_eq!(
        ids(&forward),
        vec!["alpha", "shared", "beta"],
        "packages are grouped by root in the order the roots were given, sorted by id within a root"
    );
    let shadowed = forward
        .iter()
        .find(|package| package.id.0 == "shared")
        .expect("the duplicated id must still be discovered exactly once");
    assert_eq!(
        shadowed.root,
        PackageRoot::Directory(shared_in_first.clone()),
        "a duplicate package id in a later root must not displace the earlier root's package"
    );
    assert_eq!(
        resource_paths(shadowed),
        vec![Path::new("from-first.txt")],
        "the surviving duplicate is the earlier root's package, contents and all"
    );
    assert_eq!(
        forward.iter().filter(|package| package.id.0 == "shared").count(),
        1,
        "a shadowed duplicate is dropped, not reported twice"
    );

    let reversed = discover_ok(&loader, &[second, first]);
    assert_eq!(
        ids(&reversed),
        vec!["beta", "shared", "alpha"],
        "root precedence follows the argument order, not the alphabet"
    );
    assert_eq!(
        reversed
            .iter()
            .find(|package| package.id.0 == "shared")
            .map(|package| &package.root),
        Some(&PackageRoot::Directory(shared_in_second)),
        "reversing root precedence reverses which duplicate wins, proving precedence is real"
    );
}

#[test]
fn a_missing_unreadable_or_non_directory_root_is_skipped_without_failing_the_scan() {
    let tree = TempTree::new("bad-roots");
    let healthy = tree.root("healthy");

    let loose_dir = build_directory_package(&healthy, "loose", MINIMAL);
    let staging = build_directory_package(&tree.root("staging"), "zipped", &[("zipped.py", b"Z = 1\n")]);
    archive_from_directory(&staging, &healthy.join(archive_file_name("zipped")));

    let absent = tree.path().join("roots").join("absent");
    let plain_file = tree.path().join("roots").join("plain.txt");
    write_file(&plain_file, b"not a directory");
    let under_a_file = plain_file.join("inner");

    // An empty directory the process may not read. Empty on purpose: the scan
    // must contribute nothing from it whether the check fails with EACCES or,
    // when the suite runs as root, succeeds and finds nothing.
    let unreadable = tree.root("unreadable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
            .expect("fixture root must be made unreadable");
    }

    let loader = PackageLoader::new(tree.cache_root());
    let discovered = discover_ok(
        &loader,
        &[
            absent.clone(),
            plain_file.clone(),
            under_a_file.clone(),
            unreadable.clone(),
            healthy.clone(),
        ],
    );

    assert_eq!(
        ids(&discovered),
        vec!["loose", "zipped"],
        "an unusable root is skipped, never fatal: a missing root ({absent}), a regular file \
         ({plain_file}), a path below a regular file ({under_a_file}) and an unreadable directory \
         ({unreadable}) must all leave the healthy root's packages intact",
        absent = absent.display(),
        plain_file = plain_file.display(),
        under_a_file = under_a_file.display(),
        unreadable = unreadable.display(),
    );
    assert_eq!(
        discovered[0].root,
        PackageRoot::Directory(loose_dir),
        "a directory package in a healthy root survives its unusable siblings"
    );
    assert!(
        matches!(discovered[1].root, PackageRoot::Archive { .. }),
        "discovery finds `.{PACKAGE_ARCHIVE_EXTENSION}` archives in a root, not just directories, got {:?}",
        discovered[1].root
    );

    assert!(
        discover_ok(&loader, &[]).is_empty(),
        "scanning no roots at all yields no packages and is not an error"
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn a_package_without_a_plugin_module_is_refused_by_name_and_never_yielded_empty() {
    const NO_MODULE: &[FixtureFile] = &[
        ("lib/helpers.py", b"HELPER = 1\n"),
        ("icon.png", b"\x89PNG\r\n\x1a\n"),
        ("broken.ini", b"[main]\n"),
    ];

    let tree = TempTree::new("no-plugin-module");
    let root = tree.root("packages");
    let broken_dir = build_directory_package(&root, "broken", NO_MODULE);
    build_directory_package(&root, "healthy", MINIMAL);

    let loader = PackageLoader::new(tree.cache_root());
    let error = load_err(&loader, &broken_dir);

    let PackageError::NoPluginModule {
        package,
        root: reported,
    } = &error
    else {
        panic!("a package with no top-level module must be refused as NoPluginModule, got {error:?}");
    };
    assert_eq!(
        package.0, "broken",
        "the refusal must name the offending package so the diagnostic is actionable (spec 26.2)"
    );
    assert_eq!(
        reported, &broken_dir,
        "the refusal must carry the path the user pointed at"
    );
    assert_error_names(&error, "broken");

    let discovered = discover_ok(&loader, &[root]);
    assert_eq!(
        ids(&discovered),
        vec!["healthy"],
        "a broken package is skipped by the scan — never yielded as an empty package — and it must \
         not take its healthy siblings down with it"
    );
}

#[test]
fn an_archive_entry_that_escapes_the_package_root_is_refused_and_writes_nothing() {
    let tree = TempTree::new("path-traversal");
    let archive_path = tree.root("archived").join(archive_file_name("hostile"));

    let mut hostile = RawZip::default();
    hostile
        .push("hostile.py", &b"import keypirinha\n"[..])
        .push("../escape.py", &b"import os; os.system('rm -rf ~')\n"[..])
        .push("../../escape.py", &b"import os; os.system('rm -rf ~')\n"[..]);
    hostile.write_to(&archive_path);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::new(cache_root.clone());
    let error = load_err(&loader, &archive_path);

    let PackageError::UnsafeEntryPath { archive, entry } = &error else {
        panic!(
            "an archive entry escaping the package root must be refused as UnsafeEntryPath, got {error:?}"
        );
    };
    assert_eq!(
        archive, &archive_path,
        "the refusal must name the hostile archive so it can be quarantined"
    );
    assert_eq!(
        entry, "../escape.py",
        "the loader must validate every entry name before extracting any byte, and report the first \
         unsafe entry in archive order"
    );
    assert_error_names(&error, "../escape.py");

    let escaped = find_named(tree.path(), "escape.py");
    assert!(
        escaped.is_empty(),
        "a refused archive must write nothing anywhere: found escaped files {escaped:?}"
    );
    assert!(
        relative_files(&cache_root).is_empty(),
        "validation precedes extraction, so a refused archive leaves the cache root empty, found {:?}",
        relative_files(&cache_root)
    );
    assert!(
        find_named(tree.path(), "hostile.py").is_empty(),
        "not even the archive's legitimate entries may be extracted once one entry is hostile"
    );
}

#[test]
fn hostile_archive_path_spellings_are_refused_before_extraction() {
    for (label, hostile_name) in [
        ("absolute-entry", "/outside.py"),
        ("backslash-entry", "lib\\outside.py"),
        ("windows-device-entry", "CON.txt"),
        ("trailing-dot-entry", "payload.py."),
        // `PathBuf::push` on Windows REPLACES the accumulated path when the
        // pushed component carries a drive prefix, so a drive-relative name
        // extracts outside the cache root entirely.
        ("drive-relative-entry", "C:evil.dll"),
        ("drive-absolute-entry", "C:/evil.dll"),
        // An NTFS alternate data stream writes bytes that no later reader of
        // the visible file can see.
        ("alternate-data-stream-entry", "payload.py:hidden"),
    ] {
        let tree = TempTree::new(label);
        let archive_path = tree.root("archived").join(archive_file_name(label));

        let mut archive = RawZip::default();
        archive
            .push("valid.py", &b"import keypirinha\n"[..])
            .push(hostile_name, &b"HOSTILE = True\n"[..]);
        archive.write_to(&archive_path);

        let cache_root = tree.cache_root();
        let loader = PackageLoader::new(cache_root.clone());
        let error = load_err(&loader, &archive_path);
        let PackageError::UnsafeEntryPath { entry, .. } = &error else {
            panic!("entry name {hostile_name:?} must be refused as UnsafeEntryPath, got {error:?}");
        };
        assert_eq!(
            entry, hostile_name,
            "the refusal must preserve the declared hostile entry name"
        );
        assert!(
            relative_files(&cache_root).is_empty(),
            "unsafe entry {hostile_name:?} must be rejected before extraction"
        );
    }
}

#[test]
fn symbolic_link_archive_entries_are_refused_before_extraction() {
    let tree = TempTree::new("symlink-entry");
    let archive_path = tree.root("archived").join(archive_file_name("symlink"));

    let mut archive = RawZip::default();
    archive
        .push("symlink.py", &b"import keypirinha\n"[..])
        .push_symlink("linked.py", &b"../outside.py"[..]);
    archive.write_to(&archive_path);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::new(cache_root.clone());
    let error = load_err(&loader, &archive_path);
    let PackageError::SymlinkEntry { entry, .. } = &error else {
        panic!("symbolic-link entries must be refused as SymlinkEntry, got {error:?}");
    };
    assert_eq!(entry, "linked.py");
    assert!(
        relative_files(&cache_root).is_empty(),
        "a symbolic-link refusal must happen before extraction"
    );
}

#[test]
fn an_empty_or_non_zip_keypirinha_package_file_is_refused_as_a_malformed_archive() {
    let tree = TempTree::new("malformed-archive");
    let root = tree.root("archived");

    let empty = root.join(archive_file_name("empty"));
    let valid_empty = root.join(archive_file_name("valid-empty"));
    let not_zip = root.join(archive_file_name("nonzip"));
    write_file(&empty, b"");
    RawZip::default().write_to(&valid_empty);
    write_file(
        &not_zip,
        b"#!/usr/bin/env python3\nprint('definitely not a zip container')\n",
    );

    let cache_root = tree.cache_root();
    let loader = PackageLoader::new(cache_root.clone());

    for (path, description) in [(&empty, "a zero-byte file"), (&not_zip, "a non-ZIP file")] {
        let error = load_err(&loader, path);
        let PackageError::MalformedArchive { archive, detail } = &error else {
            panic!(
                "{description} named `.{PACKAGE_ARCHIVE_EXTENSION}` must be refused as \
                 MalformedArchive, got {error:?}"
            );
        };
        assert_eq!(
            archive, path,
            "the refusal must name the unreadable archive ({description})"
        );
        assert!(
            !detail.trim().is_empty(),
            "MalformedArchive must carry a non-empty explanation for the compatibility diagnostic \
             (spec 26.2), got {detail:?} for {description}"
        );
        assert_error_names(&error, &file_name_of(path));
    }

    let error = load_err(&loader, &valid_empty);
    let PackageError::EmptyArchive { archive } = &error else {
        panic!("a valid ZIP with no entries must be refused as EmptyArchive, got {error:?}");
    };
    assert_eq!(
        archive, &valid_empty,
        "the empty-archive refusal must name the archive"
    );
    assert_error_names(&error, &file_name_of(&valid_empty));

    let discovered = discover_ok(&loader, &[root]);
    assert!(
        discovered.is_empty(),
        "unreadable archives are skipped by the scan rather than failing it, got {:?}",
        ids(&discovered)
    );
    assert!(
        relative_files(&cache_root).is_empty(),
        "a container that cannot be opened must leave nothing behind in the cache root, found {:?}",
        relative_files(&cache_root)
    );
}

#[test]
fn a_corrupt_entry_payload_is_reported_as_a_malformed_archive_without_cache_output() {
    let tree = TempTree::new("corrupt-payload");
    let archive_path = tree.root("archived").join(archive_file_name("corrupt"));
    let mut archive = RawZip::default();
    archive.push("corrupt.py", &b"import keypirinha\n"[..]);
    archive.write_to(&archive_path);

    // Keep the central directory and CRC untouched, but flip one byte in the
    // stored local payload. The ZIP container remains structurally indexed;
    // decompression must report the checksum failure while extraction is in
    // progress.
    let mut bytes = fs::read(&archive_path).expect("raw archive must be readable");
    let payload_offset = 30 + "corrupt.py".len();
    bytes[payload_offset] ^= 0xff;
    write_file(&archive_path, &bytes);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::new(cache_root.clone());
    let error = load_err(&loader, &archive_path);
    assert!(
        matches!(error, PackageError::MalformedArchive { .. }),
        "a CRC failure must be a clean malformed-archive error, got {error:?}"
    );
    assert!(
        relative_files(&cache_root).is_empty(),
        "a corrupt payload must remove its staging directory rather than leave partial output, found {:?}",
        relative_files(&cache_root)
    );
}

#[test]
fn an_entry_name_that_is_not_valid_utf8_is_refused_rather_than_lossily_decoded() {
    const BAD_NAME: &[u8] = b"lib/bad\xFFname.py";

    let tree = TempTree::new("non-utf8-entry");
    let archive_path = tree.root("archived").join(archive_file_name("mojibake"));

    let mut malformed = RawZip::default();
    malformed
        .push("mojibake.py", &b"import keypirinha\n"[..])
        .push(BAD_NAME, &b"BAD = 1\n"[..]);
    malformed.write_to(&archive_path);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::new(cache_root.clone());
    // The trap this test exists to catch: `zip` accepts this archive happily.
    // `ZipArchive::new` does not validate name encoding, and `ZipFile::name()`
    // hands back `lib/bad<U+FFFD>name.py` — the bad byte silently replaced. A
    // loader that trusts `name()` extracts a file whose path is not what the
    // archive said. The undecoded bytes are available from `ZipFile::name_raw()`,
    // and the contract below requires the loader to use them.
    // Refusing is not CriKey being pedantic: CPython's `zipfile` raises
    // UnicodeDecodeError on this same archive, and does so unconditionally —
    // its bit-11 branch is a bare decode('utf-8') with no override hook, so no
    // `metadata_encoding=` argument makes it tolerate the name (measured on
    // 3.14.4 against cp437, utf-8 and latin-1). A reference reader already
    // treats this archive as malformed; `zip` and CPython agree on the flag
    // semantics and differ only in failure mode. Tolerating the lossy name
    // would make CriKey the outlier, and a quietly corrupted one.
    let error = load_err(&loader, &archive_path);

    let PackageError::NonUtf8EntryName { archive, raw_name } = &error else {
        panic!("an entry name that is not valid UTF-8 must be refused as NonUtf8EntryName, got {error:?}");
    };
    assert_eq!(
        archive, &archive_path,
        "the refusal must name the archive carrying the undecodable entry"
    );
    assert_eq!(
        raw_name.as_slice(),
        BAD_NAME,
        "the raw entry name bytes must be preserved for the diagnostic, not lossily decoded, \
         truncated at the bad byte, or replaced with U+FFFD"
    );
    assert_error_names(&error, &file_name_of(&archive_path));

    assert!(
        relative_files(&cache_root).is_empty(),
        "an undecodable entry name is caught during validation, before any extraction, found {:?}",
        relative_files(&cache_root)
    );
}

#[test]
fn an_entry_larger_than_the_documented_size_cap_is_refused_rather_than_truncated() {
    const CAP: u64 = 1024;
    const OVERSIZED: usize = 4096;

    let defaults = PackageLimits::default();
    assert!(
        defaults.max_entry_bytes >= 1024 * 1024,
        "the default per-entry cap must be generous enough for a real icon or data file, got {}",
        defaults.max_entry_bytes
    );
    assert!(
        defaults.max_total_bytes >= defaults.max_entry_bytes,
        "the whole-package cap must not be tighter than the per-entry cap, got {} vs {}",
        defaults.max_total_bytes,
        defaults.max_entry_bytes
    );
    assert!(
        defaults.max_entries > 0,
        "the entry-count cap must admit at least one entry"
    );

    let tree = TempTree::new("oversized-entry");
    let staging = tree.root("staging");
    let package_dir = build_directory_package(&staging, "huge", &[("huge.py", b"import keypirinha\n")]);
    write_file(&package_dir.join("big.txt"), &vec![b'x'; OVERSIZED]);
    let archive_path = tree.root("archived").join(archive_file_name("huge"));
    archive_from_directory(&package_dir, &archive_path);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::with_limits(
        cache_root.clone(),
        PackageLimits {
            max_entry_bytes: CAP,
            ..PackageLimits::default()
        },
    );
    assert_eq!(
        loader.limits().max_entry_bytes,
        CAP,
        "with_limits must honour the caps it was handed"
    );

    let error = load_err(&loader, &archive_path);
    let PackageError::EntryTooLarge {
        archive,
        entry,
        size,
        limit,
    } = &error
    else {
        panic!("an entry over the configured size cap must be refused as EntryTooLarge, got {error:?}");
    };
    assert_eq!(
        archive, &archive_path,
        "the refusal must name the offending archive"
    );
    assert_eq!(
        entry, "big.txt",
        "the refusal must name the oversized entry, not just the archive"
    );
    assert_eq!(
        *size, OVERSIZED as u64,
        "the refusal must report the entry's real declared size"
    );
    assert_eq!(*limit, CAP, "the refusal must report the cap that was exceeded");

    assert!(
        relative_files(&cache_root).is_empty(),
        "an oversized entry is refused outright: nothing is written, and in particular no truncated \
         prefix of the entry is left behind, found {:?}",
        relative_files(&cache_root)
    );

    let generous = PackageLoader::new(cache_root);
    let package = load_ok(&generous, &archive_path);
    assert_package_invariants(&package);
    assert_eq!(
        resource_paths(&package),
        vec![Path::new("big.txt")],
        "the same archive loads cleanly under the default caps, proving the refusal was the cap and \
         not a malformed fixture"
    );
}
#[test]
fn duplicate_archive_entry_names_are_refused_before_extraction() {
    let tree = TempTree::new("duplicate-entry");
    let archive_path = tree.root("archived").join(archive_file_name("duplicate"));

    let mut archive = RawZip::default();
    archive
        .push("duplicate.py", &b"FIRST = True\n"[..])
        .push("duplicate.py", &b"SECOND = True\n"[..]);
    archive.write_to(&archive_path);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::new(cache_root.clone());
    let error = load_err(&loader, &archive_path);
    let PackageError::DuplicateEntryName { archive, entry } = &error else {
        panic!("duplicate ZIP entry names must be refused as DuplicateEntryName, got {error:?}");
    };
    assert_eq!(archive, &archive_path);
    assert_eq!(entry, "duplicate.py");
    assert!(
        relative_files(&cache_root).is_empty(),
        "a duplicate-entry refusal must happen before extraction, found {:?}",
        relative_files(&cache_root)
    );
}

#[test]
fn an_archive_with_too_many_entries_is_refused_before_extraction() {
    let tree = TempTree::new("too-many-entries");
    let archive_path = tree.root("archived").join(archive_file_name("many"));

    let mut archive = RawZip::default();
    archive
        .push("many.py", &b"import keypirinha\n"[..])
        .push("resource.txt", &b"resource\n"[..]);
    archive.write_to(&archive_path);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::with_limits(
        cache_root.clone(),
        PackageLimits {
            max_entries: 1,
            ..PackageLimits::default()
        },
    );
    let error = load_err(&loader, &archive_path);
    let PackageError::TooManyEntries { path, count, limit } = &error else {
        panic!("an archive over the entry-count cap must be refused as TooManyEntries, got {error:?}");
    };
    assert_eq!(path, &archive_path);
    assert_eq!(*count, 2);
    assert_eq!(*limit, 1);
    assert!(
        relative_files(&cache_root).is_empty(),
        "an entry-count refusal must happen before extraction"
    );
}

#[test]
fn archive_total_uncompressed_size_is_bounded_before_extraction() {
    let tree = TempTree::new("total-size");
    let archive_path = tree.root("archived").join(archive_file_name("total"));

    let mut archive = RawZip::default();
    archive
        .push("total.py", &b"12345678"[..])
        .push("data.bin", &b"abcdefgh"[..]);
    archive.write_to(&archive_path);

    let cache_root = tree.cache_root();
    let loader = PackageLoader::with_limits(
        cache_root.clone(),
        PackageLimits {
            max_entry_bytes: 8,
            max_total_bytes: 10,
            ..PackageLimits::default()
        },
    );
    let error = load_err(&loader, &archive_path);
    let PackageError::PackageTooLarge { archive, size, limit } = &error else {
        panic!("an archive over the total-size cap must be refused as PackageTooLarge, got {error:?}");
    };
    assert_eq!(archive, &archive_path);
    assert_eq!(*size, 16);
    assert_eq!(*limit, 10);
    assert!(
        relative_files(&cache_root).is_empty(),
        "a total-size refusal must happen before extraction"
    );
}
