//! Turning an installed `wasm` package into a supervised launch.
//!
//! There is no separate provider for WebAssembly plugins, and there must not
//! be one. A `wasm` package is served by `crikey-wasm-host`, which is an
//! ordinary supervised native plugin executable, so [`crate::native_provider`]
//! owns it end to end: same worker, same supervisor, same circuit breaker,
//! same teardown. All this module contributes is *which* executable to start
//! and *what* to hand it (ADR-0014).
//!
//! Nothing here instantiates a module, and this crate deliberately does not
//! depend on `crikey-wasm-host`: the launcher has no reason to link a
//! WebAssembly interpreter (README invariant 1; acceptance criterion 30). The
//! consequence is that the environment names and grant tokens below are a
//! process boundary contract with a second copy in
//! `crikey_wasm_host::config`, exactly as `CRIKEY_PLUGIN_ENDPOINT` is a
//! contract with the SDK. Both copies are pinned by a test.
//!
//! The checks here are the cheap ones that let a broken package be reported as
//! unavailable at load time instead of as a worker that starts and dies. The
//! authoritative refusals — module validation, ABI revision, missing exports,
//! ungranted imports — belong to the host process and arrive through the
//! worker's exit and its captured diagnostics.

use std::path::{Component, Path, PathBuf};

use crikey_plugin_model::{FilesystemAccess, FilesystemScope, Manifest, Permissions};

/// Overrides where the host executable is found. Set by tests and by an
/// operator running an uninstalled build; unset in every normal installation.
pub const ENV_HOST_OVERRIDE: &str = "CRIKEY_WASM_HOST";

/// File name of the host executable, without a platform suffix.
pub const HOST_BINARY: &str = "crikey-wasm-host";

/// Path to the `.wasm` module the host must load.
pub const ENV_MODULE: &str = "CRIKEY_WASM_MODULE";
/// Human-readable plugin name advertised in the handshake.
pub const ENV_PLUGIN_NAME: &str = "CRIKEY_WASM_PLUGIN_NAME";
/// Plugin release version advertised in the handshake.
pub const ENV_PLUGIN_VERSION: &str = "CRIKEY_WASM_PLUGIN_VERSION";
/// Advisory deadline handed to the guest with each suggestion request.
pub const ENV_SOFT_DEADLINE_MS: &str = "CRIKEY_WASM_SUGGEST_SOFT_DEADLINE_MS";
/// Enforced deadline: the guest's fuel budget and watchdog derive from it.
pub const ENV_HARD_DEADLINE_MS: &str = "CRIKEY_WASM_SUGGEST_HARD_DEADLINE_MS";
/// Maximum number of items accepted from one guest batch.
pub const ENV_MAX_ITEMS: &str = "CRIKEY_WASM_MAX_ITEMS";
/// Comma-separated granted capability tokens; absent means none.
pub const ENV_GRANTS: &str = "CRIKEY_WASM_GRANTS";

/// Grant token for the confined package-directory read import.
pub const GRANT_FILESYSTEM_READ: &str = "filesystem-read";
/// Grant token for the environment-variable read import.
pub const GRANT_ENVIRONMENT: &str = "environment";

fn host_file_name() -> String {
    format!("{HOST_BINARY}{}", std::env::consts::EXE_SUFFIX)
}

/// Locates the `crikey-wasm-host` executable.
///
/// First hit wins: [`ENV_HOST_OVERRIDE`], then beside the running executable
/// (the installed layout), then one directory above that (which is where a
/// Cargo test binary in `target/<profile>/deps` finds its siblings). The
/// search path is deliberately not consulted: the host that runs third-party
/// code is part of this installation, not whatever is first on `PATH`.
///
/// Returns `None` rather than a guess. A caller that cannot find the host must
/// report the plugin unavailable, because there is nothing that could run it.
pub fn host_executable() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os(ENV_HOST_OVERRIDE) {
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path);
    }
    let executable = std::env::current_exe().ok()?;
    let name = host_file_name();
    let directory = executable.parent()?;
    let beside = directory.join(&name);
    if beside.is_file() {
        return Some(beside);
    }
    let above = directory.parent()?.join(&name);
    above.is_file().then_some(above)
}

/// The capability tokens a manifest's permissions earn a WebAssembly guest.
///
/// Nothing is granted by default and the mapping is deliberately narrow: the
/// guest ABI exposes two host capabilities, and a permission this runtime
/// cannot honour earns nothing rather than being reported as satisfied
/// (README invariant 7). In particular a `filesystem` grant of any scope buys
/// only reads confined to the package directory, because that is all the host
/// implements; broader scopes are not honoured. See ADR-0014.
pub fn grant_tokens(permissions: &Permissions) -> Vec<&'static str> {
    let mut tokens = Vec::new();
    let readable = permissions.filesystem.iter().any(|entry| {
        entry.scope != FilesystemScope::None
            && matches!(entry.access, FilesystemAccess::Read | FilesystemAccess::ReadWrite)
    });
    if readable {
        tokens.push(GRANT_FILESYSTEM_READ);
    }
    if permissions.environment {
        tokens.push(GRANT_ENVIRONMENT);
    }
    tokens
}

/// Rejects an entrypoint that is not package content.
///
/// A `.wasm` module ships inside the package. An absolute path or a parent
/// traversal would let a manifest name a file the installer never
/// authenticated, so both are refused rather than resolved.
fn module_inside_package(directory: &Path, entrypoint: &str) -> Result<PathBuf, String> {
    let relative = Path::new(entrypoint);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "wasm entrypoint {entrypoint:?} must be a path inside the package, \
             with no absolute prefix and no parent traversal"
        ));
    }
    let module = directory.join(relative);
    if !module.is_file() {
        return Err(format!("wasm entrypoint is not a file: {}", module.display()));
    }
    Ok(module)
}

/// The executable, arguments and extra environment that serve `directory`'s
/// `wasm` package.
///
/// The module path is passed by environment rather than as an argument so it
/// cannot be confused with the host's own command line, and the deadlines and
/// item ceiling come straight from the manifest so the guest is metered
/// against what its author declared.
///
type LaunchEnvironment = Vec<(String, String)>;
type LaunchRecipe = (PathBuf, Vec<String>, LaunchEnvironment);

/// `Err` carries a reason fit for a [`crate::native_provider::NativeUnavailable`]
/// diagnostic.
pub fn launch_recipe(
    manifest: &Manifest,
    directory: &Path,
    os: &str,
    arch: &str,
    hard_deadline_ms: u64,
) -> Result<LaunchRecipe, String> {
    let entrypoint = manifest
        .entrypoint_for(os, arch)
        .map_err(|error| format!("no usable wasm entrypoint: {error}"))?;
    let module = module_inside_package(directory, entrypoint)?;
    let host = host_executable().ok_or_else(|| {
        format!(
            "{} is not installed beside the launcher, so no host can run this wasm plugin \
             (set {ENV_HOST_OVERRIDE} to point at one)",
            host_file_name()
        )
    })?;
    let module = module
        .to_str()
        .ok_or_else(|| format!("wasm module path is not valid UTF-8: {}", module.display()))?
        .to_owned();

    let soft_deadline_ms = manifest.performance.suggest_soft_timeout_ms.min(hard_deadline_ms);
    let mut environment = vec![
        (ENV_MODULE.to_owned(), module),
        (ENV_PLUGIN_NAME.to_owned(), manifest.plugin.name.clone()),
        (ENV_PLUGIN_VERSION.to_owned(), manifest.plugin.version.clone()),
        (ENV_SOFT_DEADLINE_MS.to_owned(), soft_deadline_ms.to_string()),
        (ENV_HARD_DEADLINE_MS.to_owned(), hard_deadline_ms.to_string()),
        (
            ENV_MAX_ITEMS.to_owned(),
            manifest.performance.maximum_results_per_query.to_string(),
        ),
    ];
    let tokens = grant_tokens(&manifest.permissions);
    if !tokens.is_empty() {
        environment.push((ENV_GRANTS.to_owned(), tokens.join(",")));
    }
    Ok((host, Vec::new(), environment))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crikey_plugin_model::permissions::FilesystemPermission;

    /// The second copy of this contract lives in `crikey_wasm_host::config`,
    /// which pins the identical literals. Neither crate depends on the other,
    /// so a rename that changes only one side fails one of the two tests.
    #[test]
    fn the_launch_contract_names_are_pinned() {
        assert_eq!(HOST_BINARY, "crikey-wasm-host");
        assert_eq!(ENV_HOST_OVERRIDE, "CRIKEY_WASM_HOST");
        assert_eq!(ENV_MODULE, "CRIKEY_WASM_MODULE");
        assert_eq!(ENV_PLUGIN_NAME, "CRIKEY_WASM_PLUGIN_NAME");
        assert_eq!(ENV_PLUGIN_VERSION, "CRIKEY_WASM_PLUGIN_VERSION");
        assert_eq!(ENV_SOFT_DEADLINE_MS, "CRIKEY_WASM_SUGGEST_SOFT_DEADLINE_MS");
        assert_eq!(ENV_HARD_DEADLINE_MS, "CRIKEY_WASM_SUGGEST_HARD_DEADLINE_MS");
        assert_eq!(ENV_MAX_ITEMS, "CRIKEY_WASM_MAX_ITEMS");
        assert_eq!(ENV_GRANTS, "CRIKEY_WASM_GRANTS");
        assert_eq!(GRANT_FILESYSTEM_READ, "filesystem-read");
        assert_eq!(GRANT_ENVIRONMENT, "environment");
    }

    #[test]
    fn a_manifest_that_asks_for_nothing_grants_nothing() {
        assert!(grant_tokens(&Permissions::default()).is_empty());
    }

    #[test]
    fn a_write_only_filesystem_permission_does_not_earn_the_read_import() {
        let permissions = Permissions {
            filesystem: vec![FilesystemPermission {
                scope: FilesystemScope::PluginData,
                access: FilesystemAccess::Write,
            }],
            ..Permissions::default()
        };
        assert!(grant_tokens(&permissions).is_empty());
    }

    #[test]
    fn a_none_scoped_filesystem_permission_does_not_earn_the_read_import() {
        let permissions = Permissions {
            filesystem: vec![FilesystemPermission {
                scope: FilesystemScope::None,
                access: FilesystemAccess::ReadWrite,
            }],
            ..Permissions::default()
        };
        assert!(grant_tokens(&permissions).is_empty());
    }

    #[test]
    fn declared_permissions_map_onto_the_two_tokens_the_guest_abi_defines() {
        let permissions = Permissions {
            filesystem: vec![FilesystemPermission {
                scope: FilesystemScope::PluginData,
                access: FilesystemAccess::ReadWrite,
            }],
            environment: true,
            // Nothing else this runtime can honour, so nothing else is claimed.
            network: true,
            clipboard: crikey_plugin_model::ClipboardPermission::ReadWrite,
            process: true,
            secrets: true,
            ..Permissions::default()
        };
        assert_eq!(
            grant_tokens(&permissions),
            vec![GRANT_FILESYSTEM_READ, GRANT_ENVIRONMENT]
        );
    }

    #[test]
    fn an_entrypoint_outside_the_package_is_refused() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
        for entrypoint in ["/tmp/evil.wasm", "../evil.wasm", "sub/../../evil.wasm"] {
            let error = module_inside_package(directory, entrypoint)
                .expect_err("an escaping entrypoint must be refused");
            assert!(
                error.contains("inside the package"),
                "refusal must say why: {error}"
            );
        }
    }

    #[test]
    fn an_entrypoint_that_is_not_a_file_is_refused() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
        let error =
            module_inside_package(directory, "absent.wasm").expect_err("a missing module must be refused");
        assert!(error.contains("is not a file"), "refusal must say why: {error}");
    }
}
