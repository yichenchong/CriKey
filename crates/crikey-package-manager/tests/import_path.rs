//! The plugin import path (spec 15.4).
//!
//! A modern plugin's `sys.path` is assembled by CriKey, never inherited, and
//! its order is load-bearing: plugin source first (so a plugin can shadow),
//! then its packaged modules, then its managed dependency environment, then the
//! CriKey SDK. Crucially the system-wide site-packages is NEVER on it — that
//! exclusion, together with the worker's `-S` flag, is what makes a plugin's
//! imports reproducible instead of hostage to whatever the host happens to have
//! installed.

use std::path::{Path, PathBuf};

use crikey_package_manager::{EnvironmentId, ImportPath, MaterializedEnvironment, PackageError};

fn env(site_dir: &str) -> MaterializedEnvironment {
    MaterializedEnvironment {
        id: EnvironmentId("deadbeef".to_owned()),
        site_dir: PathBuf::from(site_dir),
    }
}

#[test]
fn assemble_lays_the_entries_out_in_the_spec_order() {
    let plugin_source = Path::new("/plugins/acme/src");
    let packaged = vec![
        PathBuf::from("/plugins/acme/vendored"),
        PathBuf::from("/plugins/acme/bundled"),
    ];
    let managed = env("/cache/env-abc/site");
    let sdk = Path::new("/opt/crikey/modern-sdk");

    let import_path = ImportPath::assemble(plugin_source, &packaged, &managed, sdk);

    let expected: Vec<PathBuf> = vec![
        plugin_source.to_path_buf(),
        PathBuf::from("/plugins/acme/vendored"),
        PathBuf::from("/plugins/acme/bundled"),
        PathBuf::from("/cache/env-abc/site"),
        sdk.to_path_buf(),
    ];
    assert_eq!(
        import_path.entries, expected,
        "order is exactly [plugin source, packaged.., env.site_dir, sdk]"
    );
}

#[test]
fn assemble_with_no_packaged_modules_still_keeps_source_env_sdk_in_order() {
    let plugin_source = Path::new("/plugins/mini/src");
    let managed = env("/cache/env-xyz/site");
    let sdk = Path::new("/opt/crikey/modern-sdk");

    let import_path = ImportPath::assemble(plugin_source, &[], &managed, sdk);

    assert_eq!(
        import_path.entries,
        vec![
            plugin_source.to_path_buf(),
            PathBuf::from("/cache/env-xyz/site"),
            sdk.to_path_buf(),
        ],
        "an empty packaged slice contributes nothing but must not disturb the order"
    );
}

#[test]
fn to_pythonpath_joins_entries_with_the_os_path_list_separator() {
    let plugin_source = Path::new("/plugins/acme/src");
    let packaged = vec![PathBuf::from("/plugins/acme/vendored")];
    let managed = env("/cache/env-abc/site");
    let sdk = Path::new("/opt/crikey/modern-sdk");

    let import_path = ImportPath::assemble(plugin_source, &packaged, &managed, sdk);

    // The canonical OS path-list separator is exactly what env::join_paths uses.
    let expected =
        std::env::join_paths(&import_path.entries).expect("assembled entries contain no path-list separator");
    assert_eq!(
        import_path
            .to_pythonpath()
            .expect("ordinary paths can be encoded"),
        expected,
        "to_pythonpath must join the entries with the OS path-list separator"
    );
}

#[test]
fn to_pythonpath_never_contains_a_global_site_packages_path() {
    let plugin_source = Path::new("/plugins/acme/src");
    let packaged = vec![PathBuf::from("/plugins/acme/vendored")];
    let managed = env("/cache/env-abc/site");
    let sdk = Path::new("/opt/crikey/modern-sdk");

    let import_path = ImportPath::assemble(plugin_source, &packaged, &managed, sdk);
    let rendered = import_path
        .to_pythonpath()
        .expect("ordinary paths can be encoded")
        .to_string_lossy()
        .into_owned();

    for global in ["site-packages", "dist-packages"] {
        assert!(
            !rendered.contains(global),
            "the assembled import path must exclude global {global}, got {rendered:?}"
        );
    }
    // And it must carry every category we did supply — the env dir above all,
    // since that is how declared deps become importable (acceptance 31.19).
    assert!(
        rendered.contains("/cache/env-abc/site"),
        "the managed environment's site dir must be on the import path"
    );
}

#[test]
fn to_pythonpath_reports_a_component_containing_the_platform_separator() {
    let separator = if cfg!(windows) { ';' } else { ':' };
    let bad = PathBuf::from(format!("/plugins/bad{separator}name/src"));
    let managed = env("/cache/env/site");
    let import_path = ImportPath::assemble(&bad, &[], &managed, Path::new("/opt/crikey/sdk"));

    let error = import_path
        .to_pythonpath()
        .expect_err("a path-list separator inside one component is not encodable");
    match error {
        PackageError::InvalidImportPath(message) => {
            assert!(
                message.contains("bad"),
                "error names the offending component: {message}"
            );
            assert!(
                message.contains(separator),
                "error names the platform separator `{separator}`: {message}"
            );
        }
        other => panic!("separator failure must be InvalidImportPath, got {other:?}"),
    }
}
