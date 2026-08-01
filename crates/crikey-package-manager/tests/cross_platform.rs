//! Cross-platform packaging and portable built-ins (spec 19.1-19.3, 23.3, 23.4;
//! roadmap M6 "cross-platform packaging, portable built-ins").
//!
//! These tests drive the real packaging seam — `build_package` writes the
//! archive, `install_native` performs the platform selection, and the manifest
//! recovered from the *installed* root resolves the entrypoint. Nothing here
//! asserts against a convenience helper: every guarantee is observed through a
//! package that was actually built and actually installed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crikey_package_manager::{
    build_package, install_native, verify_package, NativePackageReport, PackageError,
};
use crikey_plugin_model::Manifest;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Scratch space and fixtures (helper style copied from `native_install.rs`)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-pkgmgr-xplat-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.join(name);
        fs::create_dir_all(&path).expect("scratch subdirectory is creatable");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct Fixture {
    dir: PathBuf,
    manifest: String,
}

#[derive(Debug)]
struct FixtureSpec<'a> {
    id: &'a str,
    version: &'a str,
    os: &'a [&'a str],
    arch: &'a [&'a str],
    /// Entrypoint declaration rendered verbatim into `[plugin]`, so a test can
    /// exercise the scalar, dotted and inline-table spellings unchanged.
    entrypoint: &'a str,
    /// `bin/<name>` payloads written into the package directory.
    binaries: &'a [(&'a str, &'a [u8])],
}

fn toml_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Writes a spec-shaped native plugin directory (spec 19.1) whose entrypoint
/// declaration and `bin/` payloads are supplied by the caller.
fn write_fixture(scratch: &Scratch, label: &str, spec: FixtureSpec<'_>) -> Fixture {
    let FixtureSpec {
        id,
        version,
        os,
        arch,
        entrypoint,
        binaries,
    } = spec;
    let dir = scratch.subdir(label);
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).expect("fixture bin directory is creatable");

    let manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"Cross Platform Fixture\"\n\
         version = \"{version}\"\n\
         runtime = \"native\"\n\
         {entrypoint}\n\n\
         [platform]\n\
         os = [{}]\n\
         arch = [{}]\n",
        toml_list(os),
        toml_list(arch)
    );
    fs::write(dir.join("crikey.toml"), manifest.as_bytes()).expect("fixture manifest is writable");
    for (name, bytes) in binaries {
        fs::write(bin_dir.join(name), bytes).expect("fixture binary is writable");
    }

    Fixture { dir, manifest }
}

fn build(scratch: &Scratch, fixture: &Fixture, label: &str) -> PathBuf {
    let archive = scratch.join(label);
    build_package(&fixture.dir, &archive).expect("fixture package builds");
    archive
}

fn allow_stop(_: &str) -> Result<(), PackageError> {
    Ok(())
}

fn install(archive: &Path, root: &Path, os: &str, arch: &str) -> Result<NativePackageReport, PackageError> {
    install_native(archive, root, os, arch, &mut allow_stop).map(|install| install.report)
}

fn package_error<T: std::fmt::Debug>(result: Result<T, PackageError>) -> PackageError {
    match result {
        Ok(value) => panic!("operation unexpectedly succeeded: {value:?}"),
        Err(error) => error,
    }
}

/// Reads the manifest back out of the *installed* tree, so entrypoint
/// resolution is observed against what installation actually produced.
fn installed_manifest(root: &Path) -> Manifest {
    let text = fs::read_to_string(root.join("crikey.toml")).expect("installed manifest is readable");
    Manifest::parse(&text).expect("installed manifest parses")
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut entries = BTreeMap::new();
    collect_tree(root, root, &mut entries);
    entries
}

fn collect_tree(base: &Path, current: &Path, entries: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(current).expect("installed directory is readable") {
        let entry = entry.expect("directory entry is readable");
        let path = entry.path();
        let relative = path
            .strip_prefix(base)
            .expect("entry is below the install root")
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            collect_tree(base, &path, entries);
        } else {
            entries.insert(relative, fs::read(&path).expect("installed file is readable"));
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

const WINDOWS_BINARY: &[u8] = b"windows-payload-0000\n";
const MACOS_BINARY: &[u8] = b"macos-payload-11\n";
const LINUX_BINARY: &[u8] = b"linux-payload-222222\n";
const PORTABLE_BINARY: &[u8] = b"portable-builtin-payload\n";

/// The three-platform package used by the selection and determinism tests: one
/// distinct payload per operating system, so a resolver that ignores its `os`
/// argument cannot pass.
fn tri_platform_spec() -> FixtureSpec<'static> {
    FixtureSpec {
        id: "dev.example.tri",
        version: "1.2.3",
        os: &["windows", "macos", "linux"],
        arch: &["x86_64"],
        entrypoint: "entrypoint.windows-x86_64 = \"bin/tool.exe\"\n\
                     entrypoint.macos-x86_64 = \"bin/tool-macos\"\n\
                     entrypoint.linux-x86_64 = \"bin/tool-linux\"",
        binaries: &[
            ("tool.exe", WINDOWS_BINARY),
            ("tool-macos", MACOS_BINARY),
            ("tool-linux", LINUX_BINARY),
        ],
    }
}

// ---------------------------------------------------------------------------
// Per-platform entrypoint selection (spec 19.3)
// ---------------------------------------------------------------------------

/// A package declaring three operating systems with a per-platform entrypoint
/// table must resolve a *different* binary for each of them, and that binary
/// must be present in the installed tree.
///
/// Kills: a resolver that returns the first table entry (or any single fixed
/// entry) for every target — the three expected paths are pairwise distinct and
/// the payload bytes differ, so a fixed answer is wrong for two of the three.
#[test]
fn a_per_platform_entrypoint_table_resolves_a_distinct_binary_for_every_declared_operating_system() {
    let scratch = Scratch::new("tri-select");
    let fixture = write_fixture(&scratch, "tri", tri_platform_spec());
    let archive = build(&scratch, &fixture, "tri.crikeypkg");

    let expected = [
        ("windows", "bin/tool.exe", WINDOWS_BINARY),
        ("macos", "bin/tool-macos", MACOS_BINARY),
        ("linux", "bin/tool-linux", LINUX_BINARY),
    ];

    let mut resolved = Vec::new();
    for (os, entrypoint, payload) in expected {
        let root = scratch.join(&format!("install-{os}"));
        install(&archive, &root, os, "x86_64")
            .unwrap_or_else(|error| panic!("package declaring os = {os:?} must install on {os}: {error}"));

        let manifest = installed_manifest(&root);
        let selected = manifest
            .entrypoint_for(os, "x86_64")
            .unwrap_or_else(|error| panic!("entrypoint for {os}-x86_64 must resolve: {error}"))
            .to_owned();
        assert_eq!(
            selected, entrypoint,
            "{os} must select its own entrypoint, not another platform's"
        );
        assert_eq!(
            fs::read(root.join(&selected)).expect("selected entrypoint exists in the installed tree"),
            payload,
            "the selected entrypoint for {os} must be that platform's payload"
        );
        resolved.push(selected);
    }

    let unique = resolved.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        3,
        "the three declared platforms must resolve to three different paths, got {resolved:?}"
    );
}

/// A Windows-only package must be refused on non-Windows targets with the
/// platform-incompatibility error, and must leave no install root behind — the
/// spec requires "reported as unavailable rather than loaded" (19.3), not
/// installed-then-broken-at-launch.
///
/// Kills: dropping the `[platform] os` check and installing anyway, and
/// reporting a generic I/O or manifest error instead of the platform verdict.
#[test]
fn a_windows_only_package_is_refused_as_platform_incompatible_on_macos_and_linux_targets() {
    let scratch = Scratch::new("windows-only");
    let fixture = write_fixture(
        &scratch,
        "winonly",
        FixtureSpec {
            id: "dev.example.winonly",
            version: "0.4.0",
            os: &["windows"],
            arch: &["x86_64"],
            entrypoint: "entrypoint.windows-x86_64 = \"bin/tool.exe\"",
            binaries: &[("tool.exe", WINDOWS_BINARY)],
        },
    );
    let archive = build(&scratch, &fixture, "winonly.crikeypkg");

    for os in ["linux", "macos"] {
        let root = scratch.join(&format!("refused-{os}"));
        let error = package_error(install(&archive, &root, os, "x86_64"));
        assert!(
            matches!(error, PackageError::IncompatiblePlatform),
            "a windows-only package on {os} must report platform incompatibility, got {error:?}"
        );
        assert!(
            !root.exists(),
            "a refused install on {os} must not create the install root"
        );
    }

    let root = scratch.join("accepted-windows");
    install(&archive, &root, "windows", "x86_64").expect("the declared platform still installs");
    assert_eq!(
        fs::read(root.join("bin/tool.exe")).expect("windows payload is installed"),
        WINDOWS_BINARY
    );
}

/// When the entrypoint table has no key for the running target, the failure
/// must *name the missing platform key* so an operator can see which build is
/// absent, and the name must be derived from the target rather than hardcoded.
///
/// Kills: the current unit `IncompatiblePlatform` message ("no binary for this
/// platform/architecture"), which tells an operator nothing about which
/// `<os>-<arch>` build the package failed to ship.
#[test]
fn a_missing_entrypoint_key_reports_a_failure_naming_the_platform_key_that_is_absent() {
    let scratch = Scratch::new("missing-key");
    let fixture = write_fixture(
        &scratch,
        "partial",
        FixtureSpec {
            id: "dev.example.partial",
            version: "2.0.0",
            os: &["windows", "macos", "linux"],
            arch: &["x86_64", "aarch64"],
            entrypoint: "entrypoint.windows-x86_64 = \"bin/tool.exe\"\n\
                         entrypoint.macos-aarch64 = \"bin/tool-macos\"",
            binaries: &[("tool.exe", WINDOWS_BINARY), ("tool-macos", MACOS_BINARY)],
        },
    );
    let archive = build(&scratch, &fixture, "partial.crikeypkg");

    for (os, arch) in [("linux", "x86_64"), ("macos", "x86_64")] {
        let root = scratch.join(&format!("missing-{os}-{arch}"));
        let error = package_error(install(&archive, &root, os, arch));
        // The typed variant and its captured fields are asserted before the
        // rendered text: a `Manifest(format!("missing {os}-{arch}"))` would
        // satisfy a message-only check while discarding the distinction
        // between "this platform is not declared" and "declared but no
        // entrypoint shipped".
        match &error {
            PackageError::MissingEntrypoint {
                os: reported_os,
                arch: reported_arch,
            } => {
                assert_eq!(reported_os, os, "the error must name the requested os");
                assert_eq!(reported_arch, arch, "the error must name the requested arch");
            }
            other => {
                panic!("a declared platform with no entrypoint must be MissingEntrypoint, got {other:?}")
            }
        }
        let message = error.to_string();
        assert!(
            message.contains(&format!("{os}-{arch}")),
            "the failure for the absent {os}-{arch} build must name that key, got {message:?}"
        );
        assert!(
            !root.exists(),
            "a refused install must not create the install root"
        );
    }

    let root = scratch.join("present-windows");
    install(&archive, &root, "windows", "x86_64").expect("a declared entrypoint key still installs");
}

/// Architecture is enforced independently of the operating system: a package
/// shipping only `aarch64` is refused on `x86_64` even though its `os` matches.
///
/// Kills: an `ensure_compatible` that checks `[platform] os` and forgets
/// `[platform] arch`, which would install an unrunnable binary.
#[test]
fn an_aarch64_only_package_is_refused_on_x86_64_even_when_the_operating_system_matches() {
    let scratch = Scratch::new("arch-only");
    let fixture = write_fixture(
        &scratch,
        "arm",
        FixtureSpec {
            id: "dev.example.arm",
            version: "1.0.0",
            os: &["linux"],
            arch: &["aarch64"],
            entrypoint: "entrypoint.linux-aarch64 = \"bin/tool-linux\"",
            binaries: &[("tool-linux", LINUX_BINARY)],
        },
    );
    let archive = build(&scratch, &fixture, "arm.crikeypkg");

    let refused = scratch.join("refused-x86_64");
    let error = package_error(install(&archive, &refused, "linux", "x86_64"));
    assert!(
        matches!(error, PackageError::IncompatiblePlatform),
        "an aarch64-only package must be platform-incompatible on x86_64, got {error:?}"
    );
    assert!(
        !refused.exists(),
        "a refused install must not create the install root"
    );

    let accepted = scratch.join("accepted-aarch64");
    install(&archive, &accepted, "linux", "aarch64").expect("the declared architecture installs");
    assert_eq!(
        fs::read(accepted.join("bin/tool-linux")).expect("aarch64 payload is installed"),
        LINUX_BINARY
    );
}

// ---------------------------------------------------------------------------
// Deterministic, platform-independent verification of shared material
// ---------------------------------------------------------------------------

/// The shared material of a multi-platform package verifies identically no
/// matter which platform key selection installed it: same archive hash, same
/// entry table, same installed bytes.
///
/// Kills: making the report or the staged tree depend on the selected target
/// (for example filtering `entries` to the selected platform, or hashing only
/// the selected binary), which would make hashes non-reproducible across CI
/// runners for one and the same archive.
#[test]
fn the_same_multi_platform_package_verifies_identically_whichever_platform_key_is_selected() {
    let scratch = Scratch::new("determinism");
    let fixture = write_fixture(&scratch, "tri", tri_platform_spec());
    let archive = build(&scratch, &fixture, "tri.crikeypkg");

    let baseline = verify_package(&archive, None).expect("archive verifies");
    assert_eq!(
        baseline.hash,
        sha256_hex(&fs::read(&archive).expect("archive bytes are readable")),
        "the reported hash must be the SHA-256 of the whole archive"
    );
    let entry_names = baseline
        .entries
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    for expected in ["crikey.toml", "bin/tool.exe", "bin/tool-macos", "bin/tool-linux"] {
        assert!(
            entry_names.contains(&expected),
            "shared material must list {expected}, got {entry_names:?}"
        );
    }
    assert_eq!(
        verify_package(&archive, Some(&baseline.hash)).expect("pinned verification succeeds"),
        baseline,
        "verification must be repeatable for identical bytes"
    );

    let mut trees = Vec::new();
    for os in ["windows", "macos", "linux"] {
        let root = scratch.join(&format!("verified-{os}"));
        let report = install(&archive, &root, os, "x86_64").expect("multi-platform package installs");
        assert_eq!(
            report, baseline,
            "the report for the {os} selection must match the platform-free verification"
        );
        trees.push((os, tree(&root)));
    }

    let (first_os, first_tree) = &trees[0];
    assert!(
        first_tree.contains_key("crikey.toml") && first_tree.len() > 1,
        "the installed tree must contain the shared material, got {:?}",
        first_tree.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        first_tree.get("crikey.toml").map(Vec::as_slice),
        Some(fixture.manifest.as_bytes()),
        "the installed manifest must be the authored manifest byte for byte"
    );
    for (os, other) in &trees[1..] {
        assert_eq!(
            other, first_tree,
            "the {os} installation must be byte-identical to the {first_os} installation"
        );
    }
}

// ---------------------------------------------------------------------------
// Portable built-ins: one entrypoint, every platform
// ---------------------------------------------------------------------------

const PORTABLE_TARGETS: [(&str, &str); 6] = [
    ("windows", "x86_64"),
    ("windows", "aarch64"),
    ("macos", "x86_64"),
    ("macos", "aarch64"),
    ("linux", "x86_64"),
    ("linux", "aarch64"),
];

/// Builds a portable package written with `entrypoint`, installs it on all six
/// declared os/arch targets and asserts each one resolves and materialises the
/// single shared binary. Returns the resolved entrypoint, which must be one and
/// the same value for every target.
fn assert_portable_installs_on_every_target(label: &str, entrypoint: &str) -> String {
    let scratch = Scratch::new(label);
    let fixture = write_fixture(
        &scratch,
        "portable",
        FixtureSpec {
            id: "dev.example.portable",
            version: "3.1.4",
            os: &["windows", "macos", "linux"],
            arch: &["x86_64", "aarch64"],
            entrypoint,
            binaries: &[("portable", PORTABLE_BINARY)],
        },
    );
    let archive = build(&scratch, &fixture, "portable.crikeypkg");

    let mut resolved: Option<String> = None;
    for (os, arch) in PORTABLE_TARGETS {
        let root = scratch.join(&format!("portable-{os}-{arch}"));
        install(&archive, &root, os, arch)
            .unwrap_or_else(|error| panic!("a portable built-in must install on {os}-{arch}: {error}"));

        let manifest = installed_manifest(&root);
        let selected = manifest
            .entrypoint_for(os, arch)
            .unwrap_or_else(|error| panic!("portable entrypoint must resolve on {os}-{arch}: {error}"))
            .to_owned();
        assert_eq!(
            fs::read(root.join(&selected)).expect("portable payload is installed"),
            PORTABLE_BINARY,
            "the portable entrypoint on {os}-{arch} must point at the shared payload"
        );
        match &resolved {
            None => resolved = Some(selected),
            Some(previous) => assert_eq!(
                &selected, previous,
                "a portable built-in must resolve the same entrypoint on every target"
            ),
        }
    }
    resolved.expect("at least one portable target was exercised")
}

/// Scalar spelling: `entrypoint = "bin/portable"` is a portable built-in and
/// installs on every declared operating system and architecture.
///
/// Kills: requiring an `<os>-<arch>` key, which would make every runtime-neutral
/// plugin unavailable everywhere.
#[test]
fn a_portable_builtin_written_with_a_scalar_entrypoint_installs_on_every_declared_target() {
    let resolved =
        assert_portable_installs_on_every_target("portable-scalar", "entrypoint = \"bin/portable\"");
    assert_eq!(resolved, "bin/portable");
}

/// Dotted spelling: `entrypoint.any = "bin/portable"` names the platform-neutral
/// key explicitly and must behave exactly like the scalar spelling.
///
/// Kills: treating `any` as an ordinary `<os>-<arch>` key that never matches.
#[test]
fn a_portable_builtin_written_with_a_dotted_any_entrypoint_key_installs_on_every_declared_target() {
    let resolved =
        assert_portable_installs_on_every_target("portable-dotted", "entrypoint.any = \"bin/portable\"");
    assert_eq!(resolved, "bin/portable");
}

/// Inline-table spelling: `entrypoint = { any = "bin/portable" }` is the same
/// declaration in TOML's other table syntax and must be accepted identically.
///
/// Kills: a manifest reader that only understands dotted keys, so a valid
/// manifest becomes unparseable or platform-incompatible depending on spelling.
#[test]
fn a_portable_builtin_written_with_an_inline_entrypoint_table_installs_on_every_declared_target() {
    let resolved = assert_portable_installs_on_every_target(
        "portable-inline",
        "entrypoint = { any = \"bin/portable\" }",
    );
    assert_eq!(resolved, "bin/portable");
}

/// The three spellings are interchangeable: a portable built-in declared any of
/// the three ways is admitted on all six targets and resolves the same path.
///
/// Kills: a partial implementation that supports one spelling and silently
/// degrades the others to "unavailable on this platform".
#[test]
fn the_scalar_dotted_and_inline_portable_spellings_are_equivalent_on_every_target() {
    let spellings = [
        ("equiv-scalar", "entrypoint = \"bin/portable\""),
        ("equiv-dotted", "entrypoint.any = \"bin/portable\""),
        ("equiv-inline", "entrypoint = { any = \"bin/portable\" }"),
    ];
    let resolved =
        spellings.map(|(label, entrypoint)| assert_portable_installs_on_every_target(label, entrypoint));
    assert_eq!(
        resolved,
        [
            "bin/portable".to_owned(),
            "bin/portable".to_owned(),
            "bin/portable".to_owned()
        ],
        "every supported spelling must resolve the one portable entrypoint"
    );
}
