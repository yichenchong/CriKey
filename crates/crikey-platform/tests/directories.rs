//! Standard directory resolution (spec 18.3).
//!
//! Every case states its convention explicitly and supplies its own
//! environment, so the Windows and macOS rules are exercised on whatever host
//! runs the suite. Nothing here reads or mutates the process environment: a
//! test that did could not run beside another one.

use crikey_platform::{DirectoryConvention, DirectoryEnvironment, PluginKind, StandardDirectories};
use std::path::{Path, PathBuf};

fn xdg_environment() -> DirectoryEnvironment {
    DirectoryEnvironment::new().set("HOME", "/home/tester")
}

fn windows_environment() -> DirectoryEnvironment {
    DirectoryEnvironment::new()
        .set("APPDATA", r"C:\Users\tester\AppData\Roaming")
        .set("LOCALAPPDATA", r"C:\Users\tester\AppData\Local")
}

fn macos_environment() -> DirectoryEnvironment {
    DirectoryEnvironment::new().set("HOME", "/Users/tester")
}

fn resolve(convention: DirectoryConvention, environment: &DirectoryEnvironment) -> StandardDirectories {
    StandardDirectories::resolve(convention, environment).expect("the directories resolve")
}

// ---------------------------------------------------------------------------
// Per-convention layouts
// ---------------------------------------------------------------------------

#[test]
fn the_xdg_defaults_are_the_documented_ones() {
    let directories = resolve(DirectoryConvention::Xdg, &xdg_environment());

    assert_eq!(directories.config_dir(), Path::new("/home/tester/.config/crikey"));
    assert_eq!(
        directories.data_dir(),
        Path::new("/home/tester/.local/share/crikey")
    );
    assert_eq!(directories.cache_dir(), Path::new("/home/tester/.cache/crikey"));
    assert_eq!(
        directories.state_dir(),
        Path::new("/home/tester/.local/state/crikey")
    );
}

#[test]
fn each_xdg_variable_overrides_only_its_own_directory() {
    let environment = xdg_environment().set("XDG_CACHE_HOME", "/fast/scratch");
    let directories = resolve(DirectoryConvention::Xdg, &environment);

    assert_eq!(directories.cache_dir(), Path::new("/fast/scratch/crikey"));
    // The others keep their defaults: setting one variable is not a statement
    // about the rest of the layout.
    assert_eq!(directories.config_dir(), Path::new("/home/tester/.config/crikey"));
    assert_eq!(
        directories.data_dir(),
        Path::new("/home/tester/.local/share/crikey")
    );
}

#[test]
fn an_empty_xdg_variable_is_unset_rather_than_the_root_directory() {
    // `XDG_CACHE_HOME=` is how a shell profile unsets a variable, and the XDG
    // specification says an empty value means "use the default". Treating it as
    // a path would put the cache in `/crikey`.
    let environment = xdg_environment().set("XDG_CACHE_HOME", "");
    let directories = resolve(DirectoryConvention::Xdg, &environment);

    assert_eq!(directories.cache_dir(), Path::new("/home/tester/.cache/crikey"));
}

#[test]
fn windows_separates_roaming_settings_from_local_derived_bytes() {
    let directories = resolve(DirectoryConvention::Windows, &windows_environment());

    // Expectations are joined rather than written as literals: `Path::join`
    // uses the host's separator, so a literal `\` comparison would assert the
    // host convention instead of the Windows layout under test.
    let roaming = Path::new(r"C:\Users\tester\AppData\Roaming").join("CriKey");
    let local = Path::new(r"C:\Users\tester\AppData\Local").join("CriKey");

    assert_eq!(directories.config_dir(), roaming);
    assert_eq!(directories.data_dir(), roaming);
    // A cache under a roaming profile would be synchronised to every machine
    // the user signs in to, which is exactly what a cache must not be.
    assert_eq!(directories.cache_dir(), local.join("Cache"));
    assert_eq!(directories.state_dir(), local.join("State"));
    assert!(!directories.cache_dir().starts_with(&roaming));
}

#[test]
fn macos_uses_library_and_keeps_state_out_of_caches() {
    let directories = resolve(DirectoryConvention::MacOs, &macos_environment());

    assert_eq!(
        directories.config_dir(),
        Path::new("/Users/tester/Library/Application Support/CriKey")
    );
    assert_eq!(
        directories.cache_dir(),
        Path::new("/Users/tester/Library/Caches/CriKey")
    );
    // The system may empty Caches whenever it likes; a startup journal that
    // lived there would lose the crash it exists to record.
    assert_eq!(
        directories.state_dir(),
        Path::new("/Users/tester/Library/Application Support/CriKey/State")
    );
    assert!(!directories
        .state_dir()
        .starts_with("/Users/tester/Library/Caches"));
}

#[test]
fn no_convention_puts_the_cache_inside_the_data_directory() {
    // A cache sweeper walks the cache root. If data lived beneath it, sweeping
    // would uninstall plugins.
    for (convention, environment) in [
        (DirectoryConvention::Xdg, xdg_environment()),
        (DirectoryConvention::Windows, windows_environment()),
        (DirectoryConvention::MacOs, macos_environment()),
    ] {
        let directories = resolve(convention, &environment);
        assert!(
            !directories.cache_dir().starts_with(directories.data_dir()),
            "{convention:?} nests the cache inside the data directory"
        );
        assert!(
            !directories.data_dir().starts_with(directories.cache_dir()),
            "{convention:?} nests the data directory inside the cache"
        );
    }
}

// ---------------------------------------------------------------------------
// Explicit overrides
// ---------------------------------------------------------------------------

#[test]
fn an_explicit_override_beats_the_platform_variables() {
    let environment = windows_environment().set("CRIKEY_CONFIG_DIR", r"D:\portable\config");
    let directories = resolve(DirectoryConvention::Windows, &environment);

    assert_eq!(directories.config_dir(), Path::new(r"D:\portable\config"));
    // An override names one directory, not the layout.
    assert_eq!(
        directories.data_dir(),
        Path::new(r"C:\Users\tester\AppData\Roaming").join("CriKey")
    );
}

#[test]
fn every_directory_can_be_overridden_independently() {
    let environment = xdg_environment()
        .set("CRIKEY_CONFIG_DIR", "/srv/config")
        .set("CRIKEY_DATA_DIR", "/srv/data")
        .set("CRIKEY_CACHE_DIR", "/srv/cache")
        .set("CRIKEY_STATE_DIR", "/srv/state");
    let directories = resolve(DirectoryConvention::Xdg, &environment);

    assert_eq!(directories.config_dir(), Path::new("/srv/config"));
    assert_eq!(directories.data_dir(), Path::new("/srv/data"));
    assert_eq!(directories.cache_dir(), Path::new("/srv/cache"));
    assert_eq!(directories.state_dir(), Path::new("/srv/state"));
}

#[test]
fn a_relative_override_is_refused_by_name() {
    // The launcher's working directory is whatever the desktop started it in,
    // so a relative override would name a different place per launch.
    let environment = xdg_environment().set("CRIKEY_DATA_DIR", "relative/plugins");
    let error = StandardDirectories::resolve(DirectoryConvention::Xdg, &environment)
        .expect_err("a relative override is refused");

    let rendered = error.to_string();
    assert!(
        rendered.contains("CRIKEY_DATA_DIR"),
        "the refusal must name the variable at fault, got: {rendered}"
    );
    assert!(
        rendered.contains("absolute"),
        "the refusal must say what was wrong with it, got: {rendered}"
    );
}

#[test]
fn a_relative_platform_variable_is_refused_rather_than_silently_joined() {
    let environment = DirectoryEnvironment::new()
        .set("APPDATA", r"Roaming")
        .set("LOCALAPPDATA", r"C:\Users\tester\AppData\Local");
    let error = StandardDirectories::resolve(DirectoryConvention::Windows, &environment)
        .expect_err("a relative APPDATA is refused");

    assert!(
        error.to_string().contains("APPDATA"),
        "the refusal must name the variable at fault, got: {error}"
    );
}

// ---------------------------------------------------------------------------
// Missing environment
// ---------------------------------------------------------------------------

#[test]
fn a_missing_variable_is_named_rather_than_defaulted() {
    // Guessing `/root` or `.` here would write a user's plugins somewhere they
    // will never look for them.
    for (convention, missing) in [
        (DirectoryConvention::Xdg, "HOME"),
        (DirectoryConvention::MacOs, "HOME"),
        (DirectoryConvention::Windows, "APPDATA"),
    ] {
        let error = StandardDirectories::resolve(convention, &DirectoryEnvironment::new())
            .expect_err("an empty environment cannot resolve");
        assert!(
            error.to_string().contains(missing),
            "{convention:?} must name `{missing}`, got: {error}"
        );
    }
}

#[test]
fn windows_names_the_local_variable_when_only_that_one_is_missing() {
    let environment = DirectoryEnvironment::new().set("APPDATA", r"C:\Users\tester\AppData\Roaming");
    let error = StandardDirectories::resolve(DirectoryConvention::Windows, &environment)
        .expect_err("LOCALAPPDATA is required too");

    assert!(
        error.to_string().contains("LOCALAPPDATA"),
        "the refusal must name the variable actually missing, got: {error}"
    );
}

// ---------------------------------------------------------------------------
// Plugin roots
// ---------------------------------------------------------------------------

#[test]
fn each_plugin_runtime_gets_its_own_root_under_the_data_directory() {
    let directories = resolve(DirectoryConvention::Xdg, &xdg_environment());

    let roots: Vec<PathBuf> = PluginKind::ALL
        .iter()
        .map(|kind| directories.plugin_dir(*kind))
        .collect();

    assert_eq!(
        roots,
        vec![
            PathBuf::from("/home/tester/.local/share/crikey/plugins/legacy"),
            PathBuf::from("/home/tester/.local/share/crikey/plugins/modern"),
            PathBuf::from("/home/tester/.local/share/crikey/plugins/native"),
        ]
    );
}

#[test]
fn no_plugin_root_contains_another() {
    // Discovery for one runtime walks its own root. If one contained another,
    // every provider would read and reject every other provider's packages.
    let directories = resolve(DirectoryConvention::Xdg, &xdg_environment());

    for outer in PluginKind::ALL {
        for inner in PluginKind::ALL {
            if outer == inner {
                continue;
            }
            assert!(
                !directories
                    .plugin_dir(inner)
                    .starts_with(directories.plugin_dir(outer)),
                "{inner:?} is nested inside {outer:?}"
            );
        }
    }
}

#[test]
fn plugin_roots_live_under_data_and_never_under_cache() {
    let directories = resolve(DirectoryConvention::Xdg, &xdg_environment());

    for kind in PluginKind::ALL {
        let root = directories.plugin_dir(kind);
        assert!(
            root.starts_with(directories.data_dir()),
            "{kind:?} must be installed under the data directory"
        );
        assert!(
            !root.starts_with(directories.cache_dir()),
            "{kind:?} must not be installed under the cache directory"
        );
    }
}
